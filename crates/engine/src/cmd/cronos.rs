//! Embedding WordCraft in a host application (CRONOS ProtocolGen and CSR): the host's permission
//! policy and the document's sections.
//!
//! **Policy.** The host decides what its user may do (custom roles live in the host's backend)
//! and hands the editor a [`Policy`]: commands it refuses, a read-only mode with exceptions, and
//! whether track changes is forced on. [`Session::run`] consults it before any command runs, so
//! the ribbon, shortcuts, menus, the control channel and the host bridge are all covered. There
//! is deliberately no command that changes it: only the host sets it ([`Session::set_policy`]).
//!
//! **Sections.** A section is a body heading and the blocks under it: up to the next heading
//! (`own`) or the next heading of the same or a higher level (`subtree`). Text before the first
//! heading is the preamble. A section can carry a stable key, kept as a hidden bookmark
//! `_Sec_<key>` around its heading's text, so renaming a heading keeps the host's comments,
//! history and provenance attached. `section.show` lays out one section alone (the host shows
//! its own table of contents beside it); edits outside it are refused while it is shown.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wordcraft_doc::para::InlineObject;
use wordcraft_doc::{Block, Comment, Document, PartKind, Pos, StoryRef};
use wordcraft_layout::outline;

use super::sel_result;
use crate::{CmdError, CmdResult, CommandSpec, Selection, Session, p};

/// What the host lets its user do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Policy {
    /// Commands refused: ids (`review.resolveComment`) or prefixes (`review.accept*`, `table.*`).
    pub deny: Vec<String>,
    /// Read only: commands that change the document are refused, except those in `allow` and
    /// typing inside a comment.
    pub read_only: bool,
    /// Changing commands still allowed while `read_only` (ids or prefixes, as in `deny`).
    pub allow: Vec<String>,
    /// Track changes stays on: every change is tracked and it can't be switched off.
    pub force_track_changes: bool,
}

impl Policy {
    /// Why the policy refuses command `id` (`mutates`: it changes the document), if it does.
    pub fn refuses(&self, id: &str, mutates: bool) -> Option<&'static str> {
        if matches_any(&self.deny, id) {
            return Some("not permitted for your role");
        }
        if self.force_track_changes && id == "review.trackChanges" {
            return Some("track changes is required for this document");
        }
        if self.read_only && mutates && !matches_any(&self.allow, id) {
            return Some("this document is read-only for you");
        }
        None
    }
}

fn matches_any(patterns: &[String], id: &str) -> bool {
    patterns.iter().any(|pat| match pat.strip_suffix('*') {
        Some(prefix) => id.starts_with(prefix),
        None => pat == id,
    })
}

/// The section shown alone (`section.show`): its heading's body block and how far it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionView {
    /// Body block of the section's heading (0 with no heading: the preamble).
    pub block: usize,
    /// The section's key when it has one: the view follows it when blocks move.
    pub key: Option<String>,
    pub subtree: bool,
}

impl Session {
    /// Replace the host's policy (see [`Policy`]).
    pub fn set_policy(&mut self, policy: Policy) {
        if policy.force_track_changes {
            self.doc.settings.track_changes = true;
        }
        self.policy = policy;
    }

    /// Why the host's policy refuses command `id`, if it does (the ribbon greys such buttons).
    pub fn refusal(&self, id: &str) -> Option<String> {
        let mutates = self.registry.get(id).is_some_and(|s| s.mutates);
        if self.policy.read_only && mutates && in_comment(self) {
            return None;
        }
        self.policy.refuses(id, mutates).map(str::to_string)
    }

    /// The body blocks the section view shows (`start..end`), if one is shown.
    pub fn section_range(&self) -> Option<(usize, usize)> {
        let v = self.section_view.as_ref()?;
        Some(section_bounds(&self.doc, v.block, v.subtree))
    }
}

/// Is the caret in a comment's text (typing there is commenting, allowed while read only)?
fn in_comment(s: &Session) -> bool {
    match s.sel.focus.story {
        StoryRef::Part(id) => s.doc.parts.get(&id).is_some_and(|p| p.kind == PartKind::Comment),
        StoryRef::Body => false,
    }
}

/// Refuse `id` if the host's policy does (called by [`Session::run`] before the command).
pub(crate) fn check_policy(s: &Session, id: &str, mutates: bool) -> Result<(), CmdError> {
    if s.policy.read_only && mutates && in_comment(s) {
        return Ok(());
    }
    match s.policy.refuses(id, mutates) {
        Some(why) => Err(CmdError::Disabled(format!("{id}: {why}"))),
        None => Ok(()),
    }
}

/// Section view: did a change stay inside the section shown? Blocks before it and after it must
/// be the same as `before`'s.
pub(crate) fn inside_section(s: &Session, before: &Document) -> bool {
    let Some(v) = &s.section_view else { return true };
    let (start, end) = section_bounds(before, v.block, v.subtree);
    let (old, new) = (&before.body, &s.doc.body);
    let tail = old.len().saturating_sub(end);
    if new.len() < start.saturating_add(tail) {
        return false;
    }
    let same = |a: &Arc<Block>, b: &Arc<Block>| Arc::ptr_eq(a, b) || a == b;
    let head_same = old.iter().take(start).zip(new.iter()).all(|(a, b)| same(a, b));
    let tail_same = old.iter().skip(end).zip(new.iter().skip(new.len() - tail)).all(|(a, b)| same(a, b));
    head_same && tail_same && before.last_section == s.doc.last_section
}

/// After a command: the section view follows its heading (by key, else the first heading at or
/// after where it was) and the caret stays inside it.
pub(crate) fn after_command(s: &mut Session) {
    let Some(v) = s.section_view.clone() else { return };
    let levels = outline::levels(&s.doc);
    let block = v
        .key
        .as_deref()
        .and_then(|k| find_key(&s.doc, k))
        .or_else(|| (v.block..levels.len()).find(|b| levels.get(*b).is_some_and(|l| *l < outline::BODY)))
        .unwrap_or(v.block.min(levels.len().saturating_sub(1)));
    if block != v.block
        && let Some(view) = s.section_view.as_mut()
    {
        view.block = block;
    }
    let Some((start, end)) = s.section_range() else { return };
    // Each end of the selection is kept inside: Select All selects the section.
    let first = s.doc.clamp(&Pos::body(start, 0));
    let last = end_of_block(&s.doc, end.saturating_sub(1));
    let clamp = |p: &Pos| match (p.story, p.path.0.first().map(|b| *b as usize)) {
        (StoryRef::Body, Some(b)) if b < start => first.clone(),
        (StoryRef::Body, Some(b)) if b >= end => last.clone(),
        _ => p.clone(),
    };
    let sel = Selection { anchor: clamp(&s.sel.anchor), focus: clamp(&s.sel.focus) };
    if sel != s.sel {
        s.sel = sel;
    }
}

/// The end of body block `block`: of its paragraph, or of a table's last paragraph.
fn end_of_block(doc: &Document, block: usize) -> Pos {
    let path = doc.para_paths(StoryRef::Body).into_iter().rev().find(|p| p.0.first().is_some_and(|b| *b as usize == block));
    match path {
        Some(path) => {
            let off = doc.para(StoryRef::Body, &path).map(|p| p.len()).unwrap_or(0);
            Pos { story: StoryRef::Body, path, off }
        }
        None => doc.clamp(&Pos::body(block, 0)),
    }
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("section.list", "Sections", "Host › Sections", list)
            .params(r#"{"text"?: bool (include each section's plain text), "subtree"?: bool (a section runs to the next heading of its level or higher; default: to the next heading)}"#)
            .pure(),
        CommandSpec::new("section.select", "Go to Section", "Host › Sections", |s, v| {
            let (start, _) = target(s, v)?;
            s.sel = Selection::caret(s.doc.clamp(&Pos::body(start, 0)));
            sel_result(s)
        })
        .params(r#"{"key"?: string, "block"?: n}"#)
        .pure(),
        CommandSpec::new("section.show", "Show Section", "Host › Sections", show)
            .params(r#"{"key"?: string, "block"?: n, "subtree"?: bool} — lay out this section alone; edits outside it are refused"#)
            .pure(),
        CommandSpec::new("section.showAll", "Show All Sections", "Host › Sections", |s, _| {
            s.section_view = None;
            sel_result(s)
        })
        .pure(),
        CommandSpec::new("section.setKey", "Set Section Key", "Host › Sections", set_key).params(r#"{"block": n (the heading's body block), "key": string (letters, digits and _, at most 35)}"#),
        CommandSpec::new("section.autoKeys", "Assign Section Keys", "Host › Sections", auto_keys),
        CommandSpec::new("section.export", "Export Section", "Host › Sections", export)
            .params(r#"{"key"?: string, "block"?: n, "subtree"?: bool, "format"?: "docx|html|md|txt|pdf" (default docx)} → {"data": base64}"#)
            .pure(),
        CommandSpec::new("section.replace", "Replace Section", "Host › Sections", replace).params(
            r#"{"key"?: string, "block"?: n, "subtree"?: bool, "data"?: base64, "name"?: string (its format by extension; default docx), "html"?: string, "markdown"?: string, "keepHeading"?: bool (default: keep ours unless the new content starts with a heading)}"#,
        ),
        CommandSpec::new("review.focusComment", "Go to Comment", "Review › Comments", focus_comment)
            .params(r#"{"id": n} — select the text a comment (or the thread a reply is in) is on"#)
            .pure(),
    ]
}

/// The prefix of the hidden bookmarks carrying section keys.
const KEY_PREFIX: &str = "_Sec_";
/// Word bookmark names have at most 40 characters.
const MAX_KEY: usize = 40 - KEY_PREFIX.len();

/// `key` as a bookmark-safe section key (letters, digits, `_`), or `None` if nothing is left.
fn clean_key(key: &str) -> Option<String> {
    let k: String = key.trim().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).take(MAX_KEY).collect();
    (!k.trim_matches('_').is_empty()).then_some(k)
}

/// The section key on the paragraph of body block `block`.
fn key_of(doc: &Document, block: usize) -> Option<String> {
    let p = doc.body.get(block)?.as_para()?;
    p.objects.iter().find_map(|o| match o {
        InlineObject::BookmarkStart { name } => name.strip_prefix(KEY_PREFIX).map(str::to_string),
        _ => None,
    })
}

/// The body block whose paragraph carries section key `key`.
fn find_key(doc: &Document, key: &str) -> Option<usize> {
    (0..doc.body.len()).find(|b| key_of(doc, *b).as_deref() == Some(key))
}

/// The blocks of the section whose heading is body block `block`: `block..end`. A block that
/// isn't a heading starts the preamble (or a run of text) that ends at the next heading.
fn section_bounds(doc: &Document, block: usize, subtree: bool) -> (usize, usize) {
    let levels = outline::levels(doc);
    let n = levels.len();
    let start = block.min(n.saturating_sub(1));
    let level = levels.get(start).copied().unwrap_or(outline::BODY);
    let end = (start + 1..n)
        .find(|b| levels.get(*b).is_some_and(|l| if subtree && level < outline::BODY { *l <= level } else { *l < outline::BODY }))
        .unwrap_or(n);
    (start, end.max(start + 1).min(n.max(start + 1)))
}

/// The section a command names (`key` or `block`): its `start..end` blocks.
fn target(s: &Session, v: &Value) -> Result<(usize, usize), CmdError> {
    let subtree = p::bool(v, "subtree").unwrap_or(false);
    let block = if let Some(k) = p::str(v, "key") {
        find_key(&s.doc, k).ok_or_else(|| CmdError::Params(format!("no section has key `{k}`")))?
    } else if let Some(b) = p::u64(v, "block") {
        let b = usize::try_from(b).unwrap_or(usize::MAX);
        if b >= s.doc.body.len() {
            return Err(CmdError::Params("`block` is past the end of the document".into()));
        }
        b
    } else {
        return Err(CmdError::Params("`key` or `block` is required".into()));
    };
    Ok(section_bounds(&s.doc, block, subtree))
}

/// The plain text of a block: a table's cells by tabs, its rows by line breaks.
fn block_text(b: &Block, out: &mut String) {
    match b {
        Block::Para(p) => out.push_str(&p.plain_text()),
        Block::Table(t) => {
            for (r, row) in t.rows.iter().enumerate() {
                if r > 0 {
                    out.push('\n');
                }
                for (c, cell) in row.cells.iter().enumerate() {
                    if c > 0 {
                        out.push('\t');
                    }
                    for (i, b) in cell.blocks.iter().enumerate() {
                        if i > 0 {
                            out.push(' ');
                        }
                        block_text(b, out);
                    }
                }
            }
        }
    }
}

fn list(s: &mut Session, v: &Value) -> CmdResult {
    let text = p::bool(v, "text").unwrap_or(false);
    let subtree = p::bool(v, "subtree").unwrap_or(false);
    let levels = outline::levels(&s.doc);
    let mut starts: Vec<usize> = levels.iter().enumerate().filter(|(_, l)| **l < outline::BODY).map(|(b, _)| b).collect();
    let preamble = starts.first().is_none_or(|first| *first > 0);
    if preamble {
        starts.insert(0, 0);
    }
    let shown = s.section_view.as_ref().map(|v| v.block);
    let sections: Vec<Value> = starts
        .into_iter()
        .map(|b| {
            let (_, end) = section_bounds(&s.doc, b, false);
            let (_, subtree_end) = section_bounds(&s.doc, b, true);
            let level = levels.get(b).copied().filter(|l| *l < outline::BODY);
            let title =
                level.and_then(|_| s.doc.body.get(b)).and_then(|x| x.as_para()).map(|p| p.plain_text().trim().to_string()).unwrap_or_default();
            let mut o = json!({
                "key": key_of(&s.doc, b),
                "level": level.unwrap_or(0),
                "title": title,
                "block": b,
                "end": end,
                "subtreeEnd": subtree_end,
                "preamble": level.is_none(),
                "shown": shown == Some(b),
            });
            if text {
                let to = if subtree { subtree_end } else { end };
                let mut t = String::new();
                for (i, blk) in s.doc.body.iter().enumerate().take(to).skip(b) {
                    if i > b {
                        t.push('\n');
                    }
                    block_text(blk, &mut t);
                }
                o["text"] = Value::String(t);
            }
            o
        })
        .collect();
    Ok(json!({"sections": sections}))
}

fn show(s: &mut Session, v: &Value) -> CmdResult {
    let (start, end) = target(s, v)?;
    s.section_view = Some(SectionView { block: start, key: key_of(&s.doc, start), subtree: p::bool(v, "subtree").unwrap_or(false) });
    s.sel = Selection::caret(s.doc.clamp(&Pos::body(start, 0)));
    Ok(json!({"block": start, "end": end}))
}

/// Put `key` on the heading paragraph of body block `block` (replacing its key and any other
/// section's use of the same key).
fn put_key(s: &mut Session, block: usize, key: &str) -> Result<(), CmdError> {
    if !matches!(s.doc.body.get(block).map(|b| &**b), Some(Block::Para(_))) {
        return Err(CmdError::Params("a section key goes on a paragraph".into()));
    }
    if let Some(old) = key_of(&s.doc, block) {
        super::insert::remove_bookmark(s, &format!("{KEY_PREFIX}{old}"))?;
    }
    let name = format!("{KEY_PREFIX}{key}");
    super::insert::remove_bookmark(s, &name)?;
    let len = s.doc.body.get(block).and_then(|b| b.as_para()).map(|p| p.len()).unwrap_or(0);
    let props = wordcraft_doc::CharProps::default();
    s.doc.insert_object(&Pos::body(block, len), InlineObject::BookmarkEnd { name: name.clone() }, &props)?;
    s.doc.insert_object(&Pos::body(block, 0), InlineObject::BookmarkStart { name }, &props)?;
    Ok(())
}

fn set_key(s: &mut Session, v: &Value) -> CmdResult {
    let block = p::u64(v, "block").and_then(|b| usize::try_from(b).ok()).ok_or_else(|| CmdError::Params("`block` is required".into()))?;
    let key = clean_key(p::req_str(v, "key")?).ok_or_else(|| CmdError::Params("`key` needs a letter or digit".into()))?;
    put_key(s, block, &key)?;
    Ok(json!({"block": block, "key": key}))
}

/// A key from a heading's title: lower case, words joined by `_`.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let out: String = out.trim_end_matches('_').chars().take(MAX_KEY - 4).collect();
    out.trim_end_matches('_').to_string()
}

/// Give every heading without a key (or with one an earlier heading already uses) a key of its
/// own made from its title.
fn auto_keys(s: &mut Session, _: &Value) -> CmdResult {
    let levels = outline::levels(&s.doc);
    let headings: Vec<usize> = levels.iter().enumerate().filter(|(_, l)| **l < outline::BODY).map(|(b, _)| b).collect();
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut todo = Vec::new();
    for b in &headings {
        match key_of(&s.doc, *b) {
            Some(k) if used.insert(k.clone()) => {}
            _ => todo.push(*b),
        }
    }
    let mut assigned = Vec::new();
    for b in todo {
        let title = s.doc.body.get(b).and_then(|x| x.as_para()).map(|p| p.plain_text()).unwrap_or_default();
        let base = Some(slug(&title)).filter(|k| !k.is_empty() && !k.starts_with(|c: char| c.is_ascii_digit())).unwrap_or_else(|| "sec".to_string());
        let mut key = base.clone();
        let mut n = 2u32;
        while used.contains(&key) {
            key = format!("{base}_{n}");
            n = n.saturating_add(1);
        }
        // A copy of another heading's bookmark goes; this heading gets its own.
        if let Some(old) = key_of(&s.doc, b) {
            remove_marker_in(s, b, &format!("{KEY_PREFIX}{old}"))?;
        }
        put_key(s, b, &key)?;
        used.insert(key.clone());
        assigned.push(json!({"block": b, "key": key}));
    }
    Ok(json!({"assigned": assigned}))
}

/// Remove bookmark `name`'s markers from the paragraph of body block `block` only.
fn remove_marker_in(s: &mut Session, block: usize, name: &str) -> Result<(), CmdError> {
    let path = wordcraft_doc::Path::top(block);
    let Some(p) = s.doc.para(StoryRef::Body, &path) else { return Ok(()) };
    let marks: Vec<usize> = p
        .object_offsets()
        .into_iter()
        .filter(|off| matches!(p.object_at(*off), Some(InlineObject::BookmarkStart { name: n } | InlineObject::BookmarkEnd { name: n }) if n == name))
        .collect();
    let para = s.doc.para_mut(StoryRef::Body, &path)?;
    let w = wordcraft_doc::para::OBJ.len_utf8();
    for off in marks.into_iter().rev() {
        para.delete(off, off + w)?;
    }
    Ok(())
}

/// The comment ids anchored (by their start or end) in `blocks`.
fn anchored(blocks: &[Arc<Block>]) -> BTreeSet<u32> {
    let mut ids = BTreeSet::new();
    for b in blocks {
        let mut blk = (**b).clone();
        super::paste::each_para(&mut blk, 0, &mut |p| {
            for o in &p.objects {
                if let InlineObject::CommentStart { id } | InlineObject::CommentEnd { id } = o {
                    ids.insert(*id);
                }
            }
        });
    }
    ids
}

/// `ids` with the replies to them, and replies to those.
fn with_replies(doc: &Document, ids: &BTreeSet<u32>) -> BTreeSet<u32> {
    let mut all = ids.clone();
    loop {
        let more: Vec<u32> =
            doc.comments.iter().filter(|(id, c)| !all.contains(id) && c.parent.is_some_and(|p| all.contains(&p))).map(|(id, _)| *id).collect();
        if more.is_empty() {
            return all;
        }
        all.extend(more);
    }
}

fn export(s: &mut Session, v: &Value) -> CmdResult {
    let (start, end) = target(s, v)?;
    let format = p::str(v, "format").unwrap_or("docx");
    if !matches!(format, "docx" | "html" | "md" | "txt" | "pdf" | "odt" | "rtf") {
        return Err(CmdError::Params("`format` is docx, html, md, txt, pdf, odt or rtf".into()));
    }
    let mut doc = s.doc.clone();
    let blocks: Vec<Arc<Block>> = doc.body.get(start..end).map(<[_]>::to_vec).unwrap_or_default();
    // Only this section's comments (and their replies) go along.
    let keep = with_replies(&doc, &anchored(&blocks));
    let gone: Vec<(u32, u32)> = doc.comments.iter().filter(|(id, _)| !keep.contains(id)).map(|(id, c)| (*id, c.part)).collect();
    for (id, part) in gone {
        doc.comments.remove(&id);
        doc.parts.remove(&part);
    }
    doc.body = blocks;
    doc.ensure_nonempty();
    let bytes = crate::io::save_bytes(&format!("section.{format}"), &doc).map_err(CmdError::Failed)?;
    Ok(json!({"data": super::insert::base64_encode(&bytes), "block": start, "end": end}))
}

/// Most bytes `section.replace` accepts (decoded).
const MAX_SECTION_BYTES: usize = 64 << 20;

fn replace(s: &mut Session, v: &Value) -> CmdResult {
    let (start, end) = target(s, v)?;
    let src = if let Some(d) = p::str(v, "data") {
        if d.len() / 4 * 3 > MAX_SECTION_BYTES {
            return Err(CmdError::Params("`data` is too large".into()));
        }
        let bytes = super::insert::base64_decode(d).ok_or_else(|| CmdError::Params("bad base64 `data`".into()))?;
        crate::io::open_bytes(p::str(v, "name").unwrap_or("section.docx"), &bytes).map_err(CmdError::Failed)?
    } else if let Some(h) = p::str(v, "html") {
        crate::io::open_bytes("section.html", h.as_bytes()).map_err(CmdError::Failed)?
    } else if let Some(m) = p::str(v, "markdown") {
        crate::io::open_bytes("section.md", m.as_bytes()).map_err(CmdError::Failed)?
    } else {
        return Err(CmdError::Params("`data`, `html` or `markdown` is required".into()));
    };
    let src_heading = outline::levels(&src).first().is_some_and(|l| *l < outline::BODY);
    let ours_heading = outline::levels(&s.doc).get(start).is_some_and(|l| *l < outline::BODY);
    let keep_heading = ours_heading && p::bool(v, "keepHeading").unwrap_or(!src_heading);
    let from = if keep_heading { start + 1 } else { start };
    let key = key_of(&s.doc, start);
    let src_comments = src.comments.clone();
    let src_parts = src.parts.clone();
    let mut frag = super::paste::adopt(s, src);
    // The section takes its place in our sections: its own section breaks don't come along.
    for b in &mut frag.blocks {
        super::paste::each_para(b, 0, &mut |p| p.section = None);
    }
    adopt_comments(s, &mut frag.blocks, &src_comments, &src_parts);
    // Our comments on the text that goes, with their replies, go too.
    let old: Vec<Arc<Block>> = s.doc.body.get(from..end).map(<[_]>::to_vec).unwrap_or_default();
    for id in with_replies(&s.doc, &anchored(&old)) {
        if let Some(c) = s.doc.comments.remove(&id) {
            s.doc.parts.remove(&c.part);
        }
    }
    // A section break on the text that goes stays, on the new section's last paragraph.
    let brk = old.iter().rev().find_map(|b| b.as_para().and_then(|p| p.section.clone()));
    let mut blocks: Vec<Arc<Block>> = frag.blocks.into_iter().map(Arc::new).collect();
    if let Some(sect) = brk {
        match blocks.last().map(|b| &**b) {
            Some(Block::Para(p)) if p.section.is_none() => {
                let mut p = p.clone();
                p.section = Some(sect);
                p.touch();
                if let Some(last) = blocks.last_mut() {
                    *last = wordcraft_doc::para_block(p);
                }
            }
            _ => {
                let mut p = wordcraft_doc::Paragraph::new();
                p.section = Some(sect);
                blocks.push(wordcraft_doc::para_block(p));
            }
        }
    }
    if blocks.is_empty() && !keep_heading {
        blocks.push(wordcraft_doc::para_block(wordcraft_doc::Paragraph::new()));
    }
    let added = blocks.len();
    let end = end.min(s.doc.body.len());
    s.doc.body.splice(from.min(end)..end, blocks);
    s.doc.ensure_nonempty();
    // The section keeps its key when its heading was replaced.
    if !keep_heading
        && let Some(k) = key
        && find_key(&s.doc, &k).is_none()
    {
        put_key(s, start, &k)?;
    }
    s.sel = Selection::caret(s.doc.clamp(&Pos::body(start, 0)));
    Ok(json!({"block": start, "end": from + added}))
}

/// Bring the comments anchored in `blocks` (and their replies) over from the source document:
/// each gets a new id and its own story here.
fn adopt_comments(s: &mut Session, blocks: &mut [Block], comments: &BTreeMap<u32, Comment>, parts: &BTreeMap<u32, wordcraft_doc::Part>) {
    let mut ids = BTreeSet::new();
    for b in blocks.iter_mut() {
        super::paste::each_para(b, 0, &mut |p| {
            for o in &p.objects {
                if let InlineObject::CommentStart { id } | InlineObject::CommentEnd { id } = o {
                    ids.insert(*id);
                }
            }
        });
    }
    // Replies come along with their threads.
    let mut all = ids.clone();
    loop {
        let more: Vec<u32> =
            comments.iter().filter(|(id, c)| !all.contains(id) && c.parent.is_some_and(|p| all.contains(&p))).map(|(id, _)| *id).collect();
        if more.is_empty() {
            break;
        }
        all.extend(more);
    }
    let mut map: BTreeMap<u32, u32> = BTreeMap::new();
    for id in &all {
        let Some(c) = comments.get(id) else { continue };
        let story = parts.get(&c.part).map(|p| p.blocks.clone()).unwrap_or_else(|| vec![wordcraft_doc::para_block(wordcraft_doc::Paragraph::new())]);
        let Ok(part) = s.doc.try_add_part(PartKind::Comment, story) else { continue };
        let Some(new) = wordcraft_doc::free_id(s.doc.comments.keys().next_back().copied(), s.doc.comments.keys().copied(), 0) else { continue };
        s.doc.comments.insert(new, Comment { part, ..c.clone() });
        map.insert(*id, new);
    }
    for new in map.values() {
        if let Some(c) = s.doc.comments.get_mut(new) {
            c.parent = c.parent.and_then(|p| map.get(&p).copied());
        }
    }
    for b in blocks.iter_mut() {
        super::paste::each_para(b, 0, &mut |p| {
            let mut changed = false;
            for o in p.objects.iter_mut() {
                if let InlineObject::CommentStart { id } | InlineObject::CommentEnd { id } = o {
                    // An anchor whose comment didn't come over points nowhere: id u32::MAX is
                    // never a comment, so it is dropped on save like any dangling anchor.
                    *id = map.get(id).copied().unwrap_or(u32::MAX);
                    changed = true;
                }
            }
            if changed {
                p.touch();
            }
        });
    }
}

fn focus_comment(s: &mut Session, v: &Value) -> CmdResult {
    let mut id = p::u64(v, "id").and_then(|x| u32::try_from(x).ok()).ok_or_else(|| CmdError::Params("`id` is required".into()))?;
    if !s.doc.comments.contains_key(&id) {
        return Err(CmdError::Params("no such comment".into()));
    }
    // A reply is shown on the comment it answers.
    for _ in 0..64 {
        match s.doc.comments.get(&id).and_then(|c| c.parent) {
            Some(parent) if s.doc.comments.contains_key(&parent) => id = parent,
            _ => break,
        }
    }
    let mut start = None;
    let mut end = None;
    for path in s.doc.para_paths(StoryRef::Body) {
        let Some(p) = s.doc.para(StoryRef::Body, &path) else { continue };
        for off in p.object_offsets() {
            match p.object_at(off) {
                Some(InlineObject::CommentStart { id: x }) if *x == id => start = Some(Pos { story: StoryRef::Body, path: path.clone(), off }),
                Some(InlineObject::CommentEnd { id: x }) if *x == id => end = Some(Pos { story: StoryRef::Body, path: path.clone(), off }),
                _ => {}
            }
        }
    }
    let Some(a) = start else { return Err(CmdError::Failed("the comment isn't on any text".into())) };
    let b = end.filter(|b| *b >= a).unwrap_or_else(|| a.clone());
    let block = a.path.0.first().copied().unwrap_or(0) as usize;
    // In a section view, the section the comment is in is shown.
    if let Some(view) = s.section_view.clone()
        && !s.section_range().is_some_and(|(x, y)| (x..y).contains(&block))
    {
        let levels = outline::levels(&s.doc);
        let heading = (0..=block).rev().find(|b| levels.get(*b).is_some_and(|l| *l < outline::BODY)).unwrap_or(0);
        s.section_view = Some(SectionView { block: heading, key: key_of(&s.doc, heading), subtree: view.subtree });
    }
    s.sel = Selection { anchor: a, focus: b };
    s.ui_requests.push(json!({"scrollToCaret": true}));
    Ok(json!({"id": id, "block": block}))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wordcraft_doc::Document;

    use super::*;

    /// Heading 1 "Intro", text, Heading 2 "Aims", text, Heading 1 "Methods", text.
    fn doc() -> Session {
        let mut s = Session::new(Document::from_text("Title page\nIntro\nwhy\nAims\nwhat\nMethods\nhow"));
        for (b, st) in [(1, "Heading 1"), (3, "Heading 2"), (5, "Heading 1")] {
            s.sel = Selection::caret(Pos::body(b, 0));
            s.run("styles.apply", &json!({"style": st})).unwrap();
        }
        s
    }

    fn sections(s: &mut Session) -> Vec<Value> {
        s.run("section.list", &json!({"text": true})).unwrap()["sections"].as_array().unwrap().clone()
    }

    #[test]
    fn lists_the_preamble_and_each_heading_with_its_blocks() {
        let mut s = doc();
        let l = sections(&mut s);
        let got: Vec<(String, u64, u64, u64, u64)> = l
            .iter()
            .map(|x| {
                (
                    x["title"].as_str().unwrap().into(),
                    x["level"].as_u64().unwrap(),
                    x["block"].as_u64().unwrap(),
                    x["end"].as_u64().unwrap(),
                    x["subtreeEnd"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(got, vec![("".into(), 0, 0, 1, 1), ("Intro".into(), 1, 1, 3, 5), ("Aims".into(), 2, 3, 5, 5), ("Methods".into(), 1, 5, 7, 7)]);
        assert_eq!(l[0]["preamble"], true);
        assert_eq!(l[1]["text"], "Intro\nwhy");
    }

    #[test]
    fn keys_survive_renaming_and_are_unique() {
        let mut s = doc();
        s.run("section.autoKeys", &json!({})).unwrap();
        let keys: Vec<Value> = sections(&mut s).iter().map(|x| x["key"].clone()).collect();
        assert_eq!(keys, vec![Value::Null, json!("intro"), json!("aims"), json!("methods")]);
        // Rename the heading: the key stays.
        s.sel = Selection::caret(Pos::body(3, 0));
        s.run("text.insert", &json!({"text": "Study "})).unwrap();
        assert_eq!(sections(&mut s)[2]["title"], "Study Aims");
        assert_eq!(sections(&mut s)[2]["key"], "aims");
        assert_eq!(s.doc.plain_text(StoryRef::Body).lines().nth(3), Some("Study Aims"));
        // An explicit key replaces it; a key in use moves.
        s.run("section.setKey", &json!({"block": 5, "key": "aims"})).unwrap();
        let keys: Vec<Value> = sections(&mut s).iter().map(|x| x["key"].clone()).collect();
        assert_eq!(keys, vec![Value::Null, json!("intro"), Value::Null, json!("aims")]);
        // Saved and opened again, the keys are still there.
        let bytes = crate::io::save_bytes("x.docx", &s.doc).unwrap();
        let mut again = Session::new(crate::io::open_bytes("x.docx", &bytes).unwrap());
        assert_eq!(sections(&mut again)[3]["key"], "aims");
    }

    #[test]
    fn policy_refuses_denied_commands_and_read_only_changes() {
        let mut s = doc();
        s.set_policy(Policy { deny: vec!["review.resolve*".into()], ..Default::default() });
        let id = s.run("review.newComment", &json!({"text": "check"})).unwrap()["id"].as_u64().unwrap();
        let e = s.run("review.resolveComment", &json!({"id": id})).unwrap_err();
        assert!(matches!(e, CmdError::Disabled(_)), "{e:?}");
        assert!(s.refusal("review.resolveComment").is_some());
        s.set_policy(Policy { read_only: true, allow: vec!["review.newComment".into()], ..Default::default() });
        assert!(s.run("text.insert", &json!({"text": "x"})).is_err());
        assert!(s.run("review.newComment", &json!({"text": "fine"})).is_ok());
        assert!(s.run("caret.docEnd", &json!({})).is_ok(), "moving around is fine");
        s.set_policy(Policy { force_track_changes: true, ..Default::default() });
        assert!(s.doc.settings.track_changes);
        assert!(s.run("review.trackChanges", &json!({"value": false})).is_err());
        assert!(s.doc.settings.track_changes);
    }

    #[test]
    fn section_view_shows_one_section_and_refuses_edits_outside_it() {
        let mut s = doc();
        s.run("section.show", &json!({"block": 3})).unwrap();
        assert_eq!(s.section_range(), Some((3, 5)));
        assert_eq!(s.sel.focus.path.0.first(), Some(&3));
        // Typing and Enter inside are fine.
        s.run("caret.end", &json!({})).unwrap();
        s.run("text.insert", &json!({"text": " and goals"})).unwrap();
        s.run("text.newParagraph", &json!({})).unwrap();
        assert_eq!(s.section_range(), Some((3, 6)));
        // Select All and delete would reach the other sections: refused, nothing changes.
        // Select All selects the section.
        s.run("select.all", &json!({})).unwrap();
        assert_eq!((s.sel.anchor.path.0.first(), s.sel.focus.path.0.first()), (Some(&3), Some(&5)));
        // A change that reaches the other sections is refused and changes nothing.
        s.run("caret.docStart", &json!({})).unwrap();
        s.doc.settings.track_changes = true;
        s.run("text.insert", &json!({"text": "x"})).unwrap();
        s.section_view = None;
        s.run("caret.docStart", &json!({})).unwrap();
        s.run("text.insert", &json!({"text": "y"})).unwrap();
        s.run("section.show", &json!({"block": 3})).unwrap();
        let before = s.doc.plain_text(StoryRef::Body);
        assert!(matches!(s.run("review.acceptAll", &json!({})), Err(CmdError::Disabled(_))));
        assert_eq!(s.doc.plain_text(StoryRef::Body), before);
        // Only the section's pages are laid out.
        let l = s.layout();
        assert!(!l.index.contains_key(&(StoryRef::Body, wordcraft_doc::Path::top(1))));
        assert!(l.index.contains_key(&(StoryRef::Body, wordcraft_doc::Path::top(3))));
        s.run("section.showAll", &json!({})).unwrap();
        assert!(s.layout().index.contains_key(&(StoryRef::Body, wordcraft_doc::Path::top(1))));
    }

    #[test]
    fn export_and_replace_a_section() {
        let mut s = doc();
        s.run("section.autoKeys", &json!({})).unwrap();
        // Another user's copy edits "Aims" and comments on it.
        let mut other = Session::new(s.doc.clone());
        other.sel = Selection::caret(Pos::body(4, 0));
        other.run("text.insert", &json!({"text": "Primary: "})).unwrap();
        other.sel = Selection { anchor: Pos::body(4, 0), focus: Pos::body(4, 7) };
        other.run("review.newComment", &json!({"text": "clarify"})).unwrap();
        let data = other.run("section.export", &json!({"key": "aims"})).unwrap()["data"].as_str().unwrap().to_string();
        s.run("section.replace", &json!({"key": "aims", "data": data})).unwrap();
        let text = s.doc.plain_text(StoryRef::Body);
        assert_eq!(text.lines().collect::<Vec<_>>(), vec!["Title page", "Intro", "why", "Aims", "Primary: what", "Methods", "how"]);
        assert_eq!(sections(&mut s)[2]["key"], "aims");
        assert_eq!(s.doc.comments.len(), 1);
        let c = s.doc.comments.values().next().unwrap();
        let ctext = s.doc.plain_text(StoryRef::Part(c.part));
        assert_eq!(ctext, "clarify");
        // Markdown content keeps our heading.
        s.run("section.replace", &json!({"key": "methods", "markdown": "We **measure**.\n\nThen report."})).unwrap();
        let text = s.doc.plain_text(StoryRef::Body);
        assert!(text.ends_with("Methods\nWe measure.\nThen report."), "{text}");
        // Replacing undoes in one step.
        s.run("edit.undo", &json!({})).unwrap();
        assert!(s.doc.plain_text(StoryRef::Body).ends_with("Methods\nhow"));
    }

    #[test]
    fn focus_comment_selects_its_text_and_shows_its_section() {
        let mut s = doc();
        s.sel = Selection { anchor: Pos::body(6, 0), focus: Pos::body(6, 3) };
        let id = s.run("review.newComment", &json!({"text": "how?"})).unwrap()["id"].as_u64().unwrap();
        let reply = s.run("review.reply", &json!({"id": id, "text": "measured"})).unwrap()["id"].as_u64().unwrap();
        s.run("section.show", &json!({"block": 1})).unwrap();
        let r = s.run("review.focusComment", &json!({"id": reply})).unwrap();
        assert_eq!(r["id"], id);
        assert_eq!(s.section_range(), Some((5, 7)));
        assert_eq!(s.sel.focus.path.0.first(), Some(&6));
    }
}
