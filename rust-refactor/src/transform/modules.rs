///! This is a transform for reorganizing definitions from a translated c2rust project.
///!
///! The main goal of this transform is unpollute the translated library from redefinitions.
///! What the c2rust transpiler does, is redefine every declaration from a header wherever that
///! header is included. Like so:
///! ```
///! mod buffer {
///!     struct buffer_t {
///!        data: i32,
///!     }
///! }
///! mod foo {
///!     struct buffer_t {
///!        data: i32,
///!     }
///! }
///! ```
///! ...

use rustc::session::Session;
use std::collections::{HashMap, HashSet};
use syntax::ast::*;
use syntax::codemap::{dummy_spanned, DUMMY_SP};
use syntax::parse::token::Lit::Str_;
use syntax::parse::token::Token::Literal;
use syntax::symbol::keywords;
use syntax::ptr::P;
use syntax::tokenstream::*;
use syntax::util::small_vector::SmallVector;
use syntax::visit::{self, Visitor};
use transform::Transform;

use api::*;
use ast_manip::AstEquiv;
use command::{CommandState, Registry};
use driver::{self, Phase};
use util::{IntoSymbol};

pub struct ReorganizeModules;

pub struct ModuleInformation {
    pub item_map: HashMap<NodeId, Item>,
    pub decl_destination_mod: HashMap<NodeId, NodeId>,
    pub new_names: HashMap<Ident, Ident>,
    pub stdlib_id: NodeId
}

impl ModuleInformation {
    fn new(id: NodeId) -> ModuleInformation {
        ModuleInformation {
            item_map: HashMap::new(),
            decl_destination_mod: HashMap::new(),
            new_names: HashMap::new(),
            stdlib_id: id,
        }
    }
}

impl<'ast> Visitor<'ast> for ModuleInformation {
    fn visit_item(&mut self, item: &'ast Item) {
        self.item_map.insert(item.id, item.clone());
        visit::walk_item(self, item);
    }
}

impl Transform for ReorganizeModules {
    fn transform(&self, krate: Crate, st: &CommandState, cx: &driver::Ctxt) -> Crate {
        let stdlib_id = st.next_node_id();
        // Cleanse the paths of the super or self prefix.
        let krate = fold_nodes(krate, |mut p: Path| {
            if p.segments.len() > 1 {
                p.segments.retain(|s| {
                    !(s.ident.name == keywords::Super.name() || s.ident.name == keywords::SelfValue.name())
                });
            }
            p
        });

        let mut mod_info = ModuleInformation::new(stdlib_id);
        krate.visit(&mut mod_info);

        // Match the modules, using a mapping like:
        // NodeId -> NodeId
        // The key is the id of the old item to be moved, and the value is the NodeId of the module
        // the item will be moved to.
        // TODO: Try and utilize the Visit trait, instead of using a visit_node
        visit_nodes(&krate, |item: &Item| {
            match item.node {
                // TODO: Move this into it's own function which accepts an Item and returns an
                // Optional decl_destination_mod
                ItemKind::Mod(ref m) => {
                    // All C standard library headers are going to be put into this arbitrary
                    // NodeId location.
                    for module_item in m.items.iter() {
                        match_modules(
                            &krate,
                            &module_item.id,
                            &item.id,
                            &mut mod_info,
                            cx.session(),
                        );
                    }
                },
                _ => {}
            }
        });

        // `new_module_decls`:
        // NodeId -> vec<NodeId>
        // The mapping is the destination module's `NodeId` to the items needing to be added to it.
        let new_module_decls = clean_module_items(&mod_info);

        // This is where the `old module` items get moved into the `new modules`
        let krate = fold_nodes(krate, |pi: P<Item>| match pi.node.clone() {
            ItemKind::Mod(ref m) => {
                return SmallVector::one(pi.map(|i| {
                    let mut m = m.clone();

                    if let Some(new_item_ids) = new_module_decls.get(&i.id) {
                        for new_item_id in new_item_ids.iter() {
                            if let Some(new_item) = mod_info.item_map.get(new_item_id) {
                                m.items.push(P(new_item.clone()));
                            }
                        }
                    }

                    Item {
                        node: ItemKind::Mod(m),
                        ..i
                    }
                }));
            }
            _ => {
                return SmallVector::one(pi);
            }
        });

        // insert a new module for the C standard headers
        let krate = extend_crate(krate, &new_module_decls, &mod_info);

        // We need to truncate the path from being `use self::some_h::foo;`,
        // to be `use some_h::foo;`
        let krate = fold_nodes(krate, |mut p: Path| {
            for segment in &mut p.segments {
                if let Some(new_path_segment) = mod_info.new_names.get(&segment.ident) {
                    segment.ident = *new_path_segment;
                }
            }
            p
        });

        // This will remove all the translated up modules.
        mod_info.item_map.clear();
        let krate = fold_nodes(krate, |pi: P<Item>| {
            // Remove the module, if it has the specific attribute
            if has_source_header(&pi.attrs) || is_std(&pi.attrs) {
                return SmallVector::new();
            }
            mod_info.item_map.insert(pi.id, pi.clone().into_inner());
            SmallVector::one(pi)
        });

        let krate = purge_duplicates(krate, &mod_info);

        krate
    }

    fn min_phase(&self) -> Phase {
        Phase::Phase3
    }
}

fn extend_crate(
    krate: Crate,
    new_module_decls: &HashMap<NodeId, Vec<NodeId>>,
    mod_info: &ModuleInformation
) -> Crate {
    let stdlib_id = mod_info.stdlib_id;
    if let Some(c_std_items) = new_module_decls.get(&stdlib_id) {
        let items: Vec<P<Item>> = c_std_items
            .iter()
            .map(|id| P(mod_info.item_map.get(id).unwrap().clone()))
            .collect();

        let stdlib_mod = Mod {
            inner: DUMMY_SP,
            items,
        };

        let new_item = Item {
            ident: Ident::new("stdlib".into_symbol(), DUMMY_SP),
            attrs: Vec::new(),
            id: stdlib_id,
            node: ItemKind::Mod(stdlib_mod),
            vis: dummy_spanned(VisibilityKind::Public),
            span: DUMMY_SP,
            tokens: None,
        };

        let mut krate_mod = krate.module.clone();

        krate_mod.items.push(P(new_item));
        return Crate {
            module: krate_mod,
            ..krate
        };
    }
    krate
}

fn purge_duplicates(krate: Crate, mod_info: &ModuleInformation) -> Crate {
    // TODO: Not all externs should be removed, combine this with next fold_nodes?
    let mut deleted_items = HashSet::new();
    let krate = fold_nodes(krate, |pi: P<Item>| {
        match pi.node.clone() {
            ItemKind::ForeignMod(ref fm) => {
                return SmallVector::one(pi.clone().map(|i| {
                    let mut fm = fm.clone();

                    fm.items.retain(|foreign_item| {
                        let mut result = true;
                        for item_map_item in mod_info.item_map.values() {
                            if let ItemKind::Mod(ref m) = item_map_item.node {
                                let mut contains_fm = false;
                                // TODO: figure out how to get the parent of fm w/o iterating
                                // through the module items
                                for module_item in m.items.iter() {
                                    if module_item.node.ast_equiv(&pi.node.clone()) {
                                        contains_fm = true;
                                    }
                                }

                                if contains_fm {
                                    for module_item in m.items.iter() {
                                        // compare the names of the declaration and ffi
                                        if module_item.ident == foreign_item.ident {
                                            result = false;
                                        }

                                        // Now check every item in a FM, just assure that the items
                                        // being checked are not the same.
                                        if let ItemKind::ForeignMod(ref fm_to_check) = module_item.node {
                                            if module_item.id != pi.id {
                                                for fm_item in fm_to_check.items.iter() {
                                                    if fm_item.ident == foreign_item.ident &&
                                                        !deleted_items.contains(&fm_item.id) {
                                                        result = false;
                                                        deleted_items.insert(foreign_item.id);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                            }
                        }
                        result
                    });

                    Item {
                        node: ItemKind::ForeignMod(fm),
                        ..i
                    }
                }));
            }
            _ => {
                return SmallVector::one(pi);
            }
        }
    });

    // TODO: Since we move the content of an module out into a destination module,
    // that destination module may contain a `use` statement that allowed the use of the `to move`
    // module item. If this is the case the use statement needs to be removed.
    //
    // ```
    // pub mod buffer {
    //     use buffer::buffer_t;
    //     ...
    //     pub struct buffer_t; // moved from mod buffer_h
    // }
    // ```
    let krate = fold_nodes(krate, |pi: P<Item>| match pi.node.clone() {
        ItemKind::Mod(ref m) => {
            return SmallVector::one(pi.map(|item| {
                let mut m = m.clone();
                let cloned_items = m.items.clone();
                m.items.retain(|i| {
                    let mut result = true;
                    match i.node {
                        ItemKind::Use(ref usetree) => {
                            for cloned_item in cloned_items.iter() {
                                match cloned_item.node {
                                    ItemKind::Ty(..) | ItemKind::Fn(..) | ItemKind::Struct(..) => {
                                        let item_declaration = cloned_item.ident;
                                        if usetree.prefix.segments
                                            .iter()
                                            .any(|s| s.ident == item_declaration)
                                        {
                                            result = false;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                    result
                });
                Item {
                    node: ItemKind::Mod(m),
                    ..item
                }
            }));
        }
        _ => {
            return SmallVector::one(pi);
        }
    });

    krate
}

// We should match possible modules together:
// test.rs should get the content of module test_h.
// So the hashmap should be something like "Test" => ModInfo { ..., "test_h"}
//
// TODO: Better variable naming; naming is too confusing.
fn match_modules(
    krate: &Crate,
    old_mod_item_id: &NodeId,
    old_mod_id: &NodeId,
    mod_info: &mut ModuleInformation,
    sess: &Session,
) {
    // `old_mod` is an `Item` type
    let item_map = mod_info.item_map.clone();
    if let Some(old_mod) = item_map.get(old_mod_id) {
        // all std header items will get placed into their own module
        // other items will be placed in matched module
        if is_std(&old_mod.attrs) {
            mod_info.decl_destination_mod.insert(*old_mod_item_id, mod_info.stdlib_id);
            mod_info.new_names.insert(old_mod.ident, Ident::from_str("stdlib"));
        } else if has_source_header(&old_mod.attrs) {
            visit_nodes(krate, |i: &Item| {
                match i.node {
                    ItemKind::Mod(_) => {
                        if !has_source_header(&i.attrs) {
                            let mut dest_mod_name = i.ident.clone();

                            // The main crate module is an empty string,
                            // so just give it it's original name
                            if dest_mod_name.as_str().is_empty() {
                                dest_mod_name = Ident::from_str(&get_source_file(sess));
                            }

                            // TODO: This is a simple naive heuristic,
                            // and should be improved upon.
                            if old_mod.ident.as_str().contains(&*dest_mod_name.as_str()) {
                                mod_info.decl_destination_mod.insert(*old_mod_item_id, i.id);
                                mod_info.new_names.insert(old_mod.ident.clone(), dest_mod_name);
                            }
                        }
                    },
                    _ => {}
                }
            });
        }
    }
}

// `clean_module_items` should iterate through decl_destination_mod, and if the Node has a similar `Item` within
// the destination module do not insert it into to the vector of NodeId's.
fn clean_module_items(
    mod_info: &ModuleInformation
) -> HashMap<NodeId, Vec<NodeId>> {
    let mut dest_items_map = HashMap::new();

    for (old_item_id, dest_mod_id) in mod_info.decl_destination_mod.iter() {
        let mut dest_vec = Vec::new();

        let old_item_option = mod_info.item_map.get(old_item_id);
        let dest_mod_option = mod_info.item_map.get(dest_mod_id);

        if dest_mod_option.is_some() && old_item_option.is_some() {
            let dest_mod_ = dest_mod_option.unwrap();
            let old_item = old_item_option.unwrap();

            // TODO: Change this to if let syntax
            unpack!([dest_mod_.node.clone()] ItemKind::Mod(dest_mod));
            // if the Module alrady has the item, no need to insert it.
            // '''
            // // dest_mod
            // Mod {
            //    pub struct some_struct {
            //      pub a: i32,
            //    }
            // }
            //
            // //item
            // pub struct some_struct {
            //    pub a: i32
            // } // should not be inserted
            // '''
            //
            // Use statement duplicates are taken care of here as well.
            let mut is_match = false;
            for dest_item in dest_mod.items.iter() {
                if dest_item.node.ast_equiv(&old_item.node) {
                    is_match = true;
                }
            }

            if !is_match {
                dest_vec.push(old_item.id);
            }
        } else if dest_mod_option.is_none() && old_item_option.is_some() {
            // This is for DUMMY_NODE_ID's
            let old_item = old_item_option.unwrap();
            dest_vec.push(old_item.id);
        }

        if !dest_items_map.contains_key(dest_mod_id) {
            dest_items_map.insert(*dest_mod_id, dest_vec);
        } else {
            if let Some(v) = dest_items_map.get_mut(dest_mod_id) {
                v.append(&mut dest_vec);
            }
        }
    }
    remove_duplicates(&mut dest_items_map, &mod_info.item_map);
    dest_items_map
}

// Remove any items that are duplicated throughout the process.
fn remove_duplicates(
    decl_destination_mod: &mut HashMap<NodeId, Vec<NodeId>>,
    item_map: &HashMap<NodeId, Item>,
) {
    let mut cloned_map = decl_destination_mod.clone();

    for (dest_mod_id, possible_duplicate_items_ids) in decl_destination_mod.iter_mut() {
        possible_duplicate_items_ids.retain(|item_id| {
            let cloned_item_ids = cloned_map.get_mut(&dest_mod_id).unwrap();

            let mut result = true;
            let mut id_to_remove: Option<NodeId> = None;
            for cloned_item_id in cloned_item_ids.iter() {
                // Make sure we aren't comparing the same items
                if *item_id != *cloned_item_id {
                    let item_a = item_map.get(&item_id).unwrap();
                    let item_b = item_map.get(&cloned_item_id).unwrap();

                    // There tends to be some flakyness around the `ast_equiv`,
                    // specifically when structs have corresponding fields.
                    // TODO: Fix ast_equiv, `Token` and `Symbol` seem to be the culprits.
                    if item_a.node.ast_equiv(&item_b.node) {
                        result = false;
                        id_to_remove = Some(item_id.clone());
                    }
                }
            }
            if let Some(id) = id_to_remove {
                let index = cloned_item_ids.iter().position(|&i| i == id).unwrap();
                // Remove the item that is deemed as a duplicate.
                cloned_item_ids.remove(index);
            }

            result
        });
    }
}

fn get_source_file(sess: &Session) -> String {
    let s = sess.local_crate_source_file.as_ref().cloned();
    s.unwrap().to_str().unwrap().to_string()
}

// This function is a check to ensure that the modules, we remove are ones translated.
// What this function is looking for is the ident, 'source_header'.
// Every translated file, that were translated with the correct option, should have:
// `#[cfg(not(source_header = "/some/path"))]`
fn has_source_header(attrs: &Vec<Attribute>) -> bool {
    // Recurse down the `TokenTree` till the `Token` is reached,
    // if the token contains an Ident with `source_tree`, this should be a translated
    // `old module` then.
    fn parse_token_tree(tree: &TokenTree, is_source_header: &mut bool) {
        match tree {
            TokenTree::Delimited(_, delimited) => {
                let stream = delimited.stream();
                stream.map(|tree| {
                    parse_token_tree(&tree, is_source_header);
                    tree
                });
            }
            TokenTree::Token(_, token) => {
                if token.is_ident() {
                    let (ident, _) = token.ident().unwrap();
                    if ident.as_str().contains("source_header") {
                        *is_source_header = true;
                    }
                }
            }
        }
    }

    let mut is_source_header = false;
    for attr in attrs {
        let tokens = attr.tokens.clone();
        tokens.map(|tree| {
            parse_token_tree(&tree, &mut is_source_header);
            tree
        });
    }
    is_source_header
}

fn is_std(attrs: &Vec<Attribute>) -> bool {
    // Recurse down the `TokenTree` till the `Token` is reached,
    // if the token contains an Ident with `source_tree`, this should be a translated
    // `old module` then.
    fn parse_token_tree(tree: &TokenTree, is_std: &mut bool) {
        match tree {
            TokenTree::Delimited(_, delimited) => {
                let stream = delimited.stream();
                stream.map(|tree| {
                    parse_token_tree(&tree, is_std);
                    tree
                });
            }
            TokenTree::Token(_, token) => match token {
                Literal(lit, _) => match lit {
                    Str_(name) => {
                        if name.as_str().contains("/usr/include") || name.as_str().contains("stddef")
                           || name.as_str().contains("vararg") {
                            *is_std = true;
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
        }
    }

    let mut is_std = false;
    for attr in attrs {
        let tokens = attr.tokens.clone();
        tokens.map(|tree| {
            parse_token_tree(&tree, &mut is_std);
            tree
        });
    }
    is_std
}

pub fn register_commands(reg: &mut Registry) {
    use super::mk;

    reg.register("reorganize_modules", |_args| mk(ReorganizeModules))
}
