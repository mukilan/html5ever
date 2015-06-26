// Copyright 2014 The html5ever Project Developers. See the
// COPYRIGHT file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A simple DOM where every node is owned by its parent.
//!
//! Since ownership is more complicated during parsing, we actually
//! build a different type and then transmute to the public `Node`.
//! This is believed to be memory safe, but if you want to be extra
//! careful you can use `RcDom` instead.
//!
//! **Warning: Unstable.** This module uses unsafe code, has not
//! been thoroughly audited, and the performance gains vs. RcDom
//! have not been demonstrated.

use common::{NodeEnum, Document, Doctype, Text, Comment, Element};

use html5ever::tokenizer::Attribute;
use html5ever::tree_builder::{TreeSink, QuirksMode, NodeOrText, AppendNode, AppendText};
use html5ever::tree_builder;
use html5ever::serialize::{Serializable, Serializer};
use html5ever::serialize::TraversalScope;
use html5ever::serialize::TraversalScope::{IncludeNode, ChildrenOnly};
use html5ever::driver::ParseResult;

use std::{mem, ptr};
use std::cell::UnsafeCell;
use std::default::Default;
use std::mem::transmute;
use std::borrow::Cow;
use std::io::{self, Write};
use std::collections::HashSet;
use std::ops::{Deref, DerefMut};

use string_cache::QualName;
use tendril::StrTendril;

/// The internal type we use for nodes during parsing.
pub struct SquishyNode {
    node: NodeEnum,
    parent: Handle,
    children: Vec<Handle>,
}

impl SquishyNode {
    fn new(node: NodeEnum) -> SquishyNode {
        SquishyNode {
            node: node,
            parent: Handle::null(),
            children: vec!(),
        }
    }
}

pub struct Handle {
    ptr: *const UnsafeCell<SquishyNode>,
}

impl Handle {
    fn new(ptr: *const UnsafeCell<SquishyNode>) -> Handle {
        Handle {
            ptr: ptr,
        }
    }

    fn null() -> Handle {
        Handle::new(ptr::null())
    }

    fn is_null(&self) -> bool {
        self.ptr.is_null()
    }
}

impl PartialEq for Handle {
    fn eq(&self, other: &Handle) -> bool {
        self.ptr == other.ptr
    }
}

impl Eq for Handle { }

impl Clone for Handle {
    fn clone(&self) -> Handle {
        Handle::new(self.ptr)
    }
}

impl Copy for Handle { }

// The safety of `Deref` and `DerefMut` depends on the invariant that `Handle`s
// can't escape the `Sink`, because nodes are deallocated by consuming the
// `Sink`.

impl DerefMut for Handle {
    fn deref_mut<'a>(&'a mut self) -> &'a mut SquishyNode {
        unsafe {
            transmute::<_, &'a mut SquishyNode>((*self.ptr).get())
        }
    }
}

impl Deref for Handle {
    type Target = SquishyNode;
    fn deref<'a>(&'a self) -> &'a SquishyNode {
        unsafe {
            transmute::<_, &'a SquishyNode>((*self.ptr).get())
        }
    }
}

fn append(mut new_parent: Handle, mut child: Handle) {
    new_parent.children.push(child);
    let parent = &mut child.parent;
    assert!(parent.is_null());
    *parent = new_parent
}

fn get_parent_and_index(child: Handle) -> Option<(Handle, usize)> {
    if child.parent.is_null() {
        return None;
    }

    let to_find = child;
    match child.parent.children.iter().enumerate().find(|&(_, n)| *n == to_find) {
        Some((i, _)) => Some((child.parent, i)),
        None => panic!("have parent but couldn't find in parent's children!"),
    }
}

fn append_to_existing_text(mut prev: Handle, text: &str) -> bool {
    match prev.deref_mut().node {
        Text(ref mut existing) => {
            existing.push_slice(text);
            true
        }
        _ => false,
    }
}

pub struct Sink {
    nodes: Vec<Box<UnsafeCell<SquishyNode>>>,
    document: Handle,
    errors: Vec<Cow<'static, str>>,
    quirks_mode: QuirksMode,
}

impl Default for Sink {
    fn default() -> Sink {
        let mut sink = Sink {
            nodes: vec!(),
            document: Handle::null(),
            errors: vec!(),
            quirks_mode: tree_builder::NoQuirks,
        };
        sink.document = sink.new_node(Document);
        sink
    }
}

impl Sink {
    fn new_node(&mut self, node: NodeEnum) -> Handle {
        self.nodes.push(box UnsafeCell::new(SquishyNode::new(node)));
        let ptr: *const UnsafeCell<SquishyNode> = &**self.nodes.last().unwrap();
        Handle::new(ptr)
    }

    // FIXME(rust-lang/rust#18296): This is separate from remove_from_parent so
    // we can call it.
    fn unparent(&mut self, mut target: Handle) {
        let (mut parent, i) = unwrap_or_return!(get_parent_and_index(target), ());
        parent.children.remove(i);
        target.parent = Handle::null();
    }
}

impl TreeSink for Sink {
    type Handle = Handle;

    fn parse_error(&mut self, msg: Cow<'static, str>) {
        self.errors.push(msg);
    }

    fn get_document(&mut self) -> Handle {
        self.document
    }

    fn set_quirks_mode(&mut self, mode: QuirksMode) {
        self.quirks_mode = mode;
    }

    fn same_node(&self, x: Handle, y: Handle) -> bool {
        x == y
    }

    fn same_home_subtree(&self, _x: Handle, _y: Handle) -> bool {
        true
    }

    fn associate_with_form(&mut self, _target: Handle, _form: Handle) {
    }

    fn has_parent_node(&self, node: Handle) -> bool {
        !node.parent.is_null()
    }

    fn elem_name(&self, target: Handle) -> QualName {
        match target.node {
            Element(ref name, _) => name.clone(),
            _ => panic!("not an element!"),
        }
    }

    fn create_element(&mut self, name: QualName, attrs: Vec<Attribute>) -> Handle {
        self.new_node(Element(name, attrs))
    }

    fn create_comment(&mut self, text: StrTendril) -> Handle {
        self.new_node(Comment(text))
    }

    fn append(&mut self, parent: Handle, child: NodeOrText<Handle>) {
        // Append to an existing Text node if we have one.
        match child {
            AppendText(ref text) => match parent.children.last() {
                Some(h) => if append_to_existing_text(*h, &text) { return; },
                _ => (),
            },
            _ => (),
        }

        append(parent, match child {
            AppendText(text) => self.new_node(Text(text)),
            AppendNode(node) => node
        });
    }

    fn append_before_sibling(&mut self,
            sibling: Handle,
            child: NodeOrText<Handle>) {
        let (mut parent, i) = get_parent_and_index(sibling)
            .expect("append_before_sibling called on node without parent");

        let mut child = match (child, i) {
            // No previous node.
            (AppendText(text), 0) => self.new_node(Text(text)),

            // Look for a text node before the insertion point.
            (AppendText(text), i) => {
                let prev = parent.children[i-1];
                if append_to_existing_text(prev, &text) {
                    return;
                }
                self.new_node(Text(text))
            }

            // The tree builder promises we won't have a text node after
            // the insertion point.

            // Any other kind of node.
            (AppendNode(node), _) => node,
        };

        if !child.parent.is_null() {
            self.unparent(child);
        }

        child.parent = parent;
        parent.children.insert(i, child);
    }

    fn append_doctype_to_document(&mut self,
                                  name: StrTendril,
                                  public_id: StrTendril,
                                  system_id: StrTendril) {
        append(self.document, self.new_node(Doctype(name, public_id, system_id)));
    }

    fn add_attrs_if_missing(&mut self, mut target: Handle, mut attrs: Vec<Attribute>) {
        let existing = match target.deref_mut().node {
            Element(_, ref mut attrs) => attrs,
            _ => return,
        };

        // FIXME: quadratic time
        attrs.retain(|attr|
            !existing.iter().any(|e| e.name == attr.name));
        existing.extend(attrs.into_iter());
    }

    fn remove_from_parent(&mut self, target: Handle) {
        self.unparent(target);
    }

    fn reparent_children(&mut self, mut node: Handle, mut new_parent: Handle) {
        new_parent.children.append(&mut node.children);
    }

    fn mark_script_already_started(&mut self, _node: Handle) { }
}

pub struct Node {
    pub node: NodeEnum,
    _parent_not_accessible: usize,
    pub children: Vec<Box<Node>>,
}

pub struct OwnedDom {
    pub document: Box<Node>,
    pub errors: Vec<Cow<'static, str>>,
    pub quirks_mode: QuirksMode,
}

impl ParseResult for OwnedDom {
    type Sink = Sink;

    fn get_result(sink: Sink) -> OwnedDom {
        fn walk(live: &mut HashSet<usize>, node: Handle) {
            live.insert(node.ptr as usize);
            for &child in node.deref().children.iter() {
                walk(live, child);
            }
        }

        // Collect addresses of all the nodes that made it into the final tree.
        let mut live = HashSet::new();
        walk(&mut live, sink.document);

        // Forget about the nodes in the final tree; they will be owned by
        // their parent.  In the process of iterating we drop all nodes that
        // aren't in the tree.
        for node in sink.nodes.into_iter() {
            let ptr: *const UnsafeCell<SquishyNode> = &*node;
            if live.contains(&(ptr as usize)) {
                mem::forget(node);
            }
        }

        let old_addrs = addrs_of!(sink.document => node, parent, children);

        // Transmute the root to a Node, finalizing the transfer of ownership.
        let document = unsafe {
            mem::transmute::<*const UnsafeCell<SquishyNode>, Box<Node>>(sink.document.ptr)
        };

        // FIXME: do this assertion statically
        let new_addrs = addrs_of!(document => node, _parent_not_accessible, children);
        assert_eq!(old_addrs, new_addrs);

        OwnedDom {
            document: document,
            errors: sink.errors,
            quirks_mode: sink.quirks_mode,
        }
    }
}

impl Serializable for Node {
    fn serialize<'wr, Wr: Write>(&self,
            serializer: &mut Serializer<'wr, Wr>,
            traversal_scope: TraversalScope) -> io::Result<()> {

        match (traversal_scope, &self.node) {
            (_, &Element(ref name, ref attrs)) => {
                if traversal_scope == IncludeNode {
                    try!(serializer.start_elem(name.clone(),
                        attrs.iter().map(|at| (&at.name, &at.value[..]))));
                }

                for child in self.children.iter() {
                    try!(child.serialize(serializer, IncludeNode));
                }

                if traversal_scope == IncludeNode {
                    try!(serializer.end_elem(name.clone()));
                }
                Ok(())
            }

            (ChildrenOnly, &Document) => {
                for child in self.children.iter() {
                    try!(child.serialize(serializer, IncludeNode));
                }
                Ok(())
            }

            (ChildrenOnly, _) => Ok(()),

            (IncludeNode, &Doctype(ref name, _, _)) => serializer.write_doctype(&name),
            (IncludeNode, &Text(ref text)) => serializer.write_text(&text),
            (IncludeNode, &Comment(ref text)) => serializer.write_comment(&text),

            (IncludeNode, &Document) => panic!("Can't serialize Document node itself"),
        }
    }
}
