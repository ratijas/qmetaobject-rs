use proc_macro::TokenStream;

use syn::{ LitStr, punctuated::Punctuated};
use syn::synom::Parser;
use std::collections::BTreeMap;
use std::fs;

#[derive(Debug)]
struct Resource {
    prefix: String,
    files: Vec<File>,
}

#[derive(Debug)]
struct File {
    file: String,
    alias: Option<String>,
}

fn parse_qrc(source : &str) -> Vec<Resource> {
    named!(file -> File,
        do_parse!(
            file: syn!(LitStr) >>
            alias: option!(do_parse!(keyword!(as) >> a:syn!(LitStr) >> (a.value()))) >>
            (File{ file: file.value(), alias })));

    named!(resource -> Resource,
        do_parse!(
            prefix: syn!(LitStr) >>
            b: braces!(call!(Punctuated::<File, Token![,]>::parse_terminated_with, file)) >>
            (Resource{ prefix: prefix.value() , files: b.1.into_iter().collect()})));

    named!(resources -> Vec<Resource>, map!(
        call!(Punctuated::<Resource, Token![,]>::parse_terminated_with, resource),
        |x| x.into_iter().collect()));

    resources.parse_str(source).expect("Cannot parse qrc macro")
}


fn qt_hash(key: &str) -> u32
{
    let mut h = 0u32;

    for p in key.chars() {
        assert_eq!(p.len_utf16(), 1, "Surrogate pair not supported by the hash function");
        h = (h << 4) + p as u32;
        h ^= (h & 0xf0000000) >> 23;
        h &= 0x0fffffff;
    }
    return h;
}

#[derive(Debug, Eq, PartialEq, PartialOrd, Ord, Clone)]
struct HashedString {
    hash: u32,
    string: String,
}
impl HashedString {
    fn new(string : String) -> HashedString {
        let hash = qt_hash(&string);
        HashedString { hash, string }
    }
}


enum TreeNode {
    File(String), // The FileName
    Directory(BTreeMap<HashedString,TreeNode>, u32)
}
impl TreeNode {
    fn new_dir() -> TreeNode {
        TreeNode::Directory(Default::default(), 0)
    }
    fn new_file(file : String) -> TreeNode {
        TreeNode::File(file)
    }

    fn insert_node(&mut self, rel_path : &str, node : TreeNode) {
        let contents = match self {
            TreeNode::Directory(ref mut contents, _) => contents,
            _ => panic!("root not a dir?"),
        };

        match rel_path.find('/') {
            Some(idx) => {
                let (name, rest) = rel_path.split_at(idx);
                let hashed = HashedString::new(name.into());
                contents.entry(hashed)
                    .or_insert_with(||{ TreeNode::new_dir() })
                    .insert_node(&rest[1..], node);
            }
            None => {
                let hashed = HashedString::new(rel_path.into());
                contents.insert(hashed, node).and_then(|_|->Option<()> { panic!("Several time the same file?") });
            }
        };
    }

    fn compute_offsets(&mut self, mut offset : u32) -> u32 {
        if let TreeNode::Directory(ref mut dir, ref mut o) = self {
            *o = offset;
            offset += dir.len() as u32;
            for (_, ref mut node) in dir {
                offset = node.compute_offsets(offset);
            }
        }
        return offset;
    }
}

fn build_tree(resources : Vec<Resource>) -> TreeNode {
    let mut root = TreeNode::new_dir();
    for r in resources {
        let mut node = TreeNode::new_dir();
        for f in r.files {
            node.insert_node(f.alias.as_ref().unwrap_or(&f.file), TreeNode::new_file(f.file.clone()));
        }
        root.insert_node(&r.prefix, node);
    }
    root
}

fn push_u32_be(v: &mut Vec<u8>, val : u32) {
    v.extend_from_slice(
        &[((val >> 24) & 0xff) as u8 , ((val >> 16) & 0xff) as u8, ((val >> 8) & 0xff) as u8, ((val >> 0) & 0xff) as u8]);
}

fn push_u16_be(v: &mut Vec<u8>, val : u16) {
    v.extend_from_slice(
        &[((val >> 8) & 0xff) as u8 , ((val >> 0) & 0xff) as u8]);
}

#[derive(Default, Debug)]
struct Data {
    payload : Vec<u8>,
    names : Vec<u8>,
    tree_data : Vec<u8>,
}
impl Data {
    fn insert_file(&mut self, filename : &str) {
        let mut data = fs::read(filename).unwrap_or_else(|_|panic!("Canot open file {}", filename));
        push_u32_be(&mut self.payload, data.len() as u32);
        self.payload.append(&mut data);
    }

    fn insert_directory(&mut self, contents: &BTreeMap<HashedString,TreeNode>) {
        for (ref name, ref val) in contents {
            let name_off = self.insert_name(name);
            push_u32_be(&mut self.tree_data, name_off);
            match val {
                TreeNode::File(ref filename) => {
                    push_u16_be(&mut self.tree_data, 0); // flags
                    push_u16_be(&mut self.tree_data, 0); // country
                    push_u16_be(&mut self.tree_data, 1); // lang (C)
                    let offset = self.payload.len();
                    push_u32_be(&mut self.tree_data, offset as u32);
                    self.insert_file(filename);
                }
                TreeNode::Directory(ref c, offset) => {
                    push_u16_be(&mut self.tree_data, 2); // directory flag
                    push_u32_be(&mut self.tree_data, c.len() as u32);
                    push_u32_be(&mut self.tree_data, *offset);
                }
            }
            // modification time (64 bit) FIXME
            push_u32_be(&mut self.tree_data, 0);
            push_u32_be(&mut self.tree_data, 0);
        }
        for (_, ref val) in contents {
            if let TreeNode::Directory(ref c, _) = val  {
                self.insert_directory(c)
            }
        }
    }

    fn insert_name(&mut self, name : &HashedString) -> u32 {
        let offset = self.names.len();
        push_u16_be(&mut self.names, name.string.len() as u16);
        push_u32_be(&mut self.names, name.hash);

        for p in name.string.chars() {
            assert_eq!(p.len_utf16(), 1, "Surrogate pair not supported");
            push_u16_be(&mut self.names, p as u16);
        }
        //println!("NAME {} -> {}", offset, name.string);
        offset as u32
    }
}

fn generate_data(root : &TreeNode) -> Data {
    let mut d = Data::default();

    let contents = match root {
        TreeNode::Directory(ref contents, _) => contents,
        _ => panic!("root not a dir?"),
    };

    // first item
    push_u32_be(&mut d.tree_data, 0); // fake name
    push_u16_be(&mut d.tree_data, 2); // flag
    push_u32_be(&mut d.tree_data, contents.len() as u32);
    push_u32_be(&mut d.tree_data, 1); // first offset
    // modification time (64 bit) FIXME
    push_u32_be(&mut d.tree_data, 0);
    push_u32_be(&mut d.tree_data, 0);

    d.insert_directory(contents);
    d
}

fn expand_macro(data : Data) -> TokenStream {
    let Data{payload, names, tree_data} = data;
    let q = quote!{
        fn register() {
            use ::std::sync::{Once, ONCE_INIT};
            static INIT_RESOURCES: Once = ONCE_INIT;
            INIT_RESOURCES.call_once(|| {
                static PAYLOAD : &'static [u8] = & [ #(#payload),* ];
                static NAMES : &'static [u8] = & [ #(#names),* ];
                static TREE_DATA : &'static [u8] = & [ #(#tree_data),* ];
                unsafe { ::qmetaobject::qrc::register_resource_data(2, TREE_DATA, NAMES, PAYLOAD) };
            });
        }
    };
    //println!("{}", q.to_string());
    q.into()
}

pub fn process_qrc(source : &str) -> TokenStream {
    let parsed = parse_qrc(source);
    let mut tree = build_tree(parsed);
    tree.compute_offsets(1);
    let d = generate_data(&tree);
    expand_macro(d)
}