# CRONOS integration (fork notes)

Status: Phase 1 foundations · branch `cronos/integration` · upstream base `9b4b948` (2026-10-19)

This fork embeds WordCraft in CRONOS ProtocolGen and CronoLex CSR. Our code is kept in its own
files so upstream can still be merged; the hooks into upstream files are a few lines each.

| Piece | Where | Doc |
|---|---|---|
| Host policy, sections, section view, comment focus | `crates/engine/src/cmd/cronos.rs` (+ hooks in `session.rs`) | this page |
| Section view layout filter | `crates/layout/src/lib.rs` (`LayoutOptions::only_blocks`) | this page |
| Compare from bytes | `crates/engine/src/cmd/tools.rs` (`review.compare` `data`/`originalData`) | this page |
| Host bridge (postMessage) and embedded mode | `apps/wordcraft-web/src/bridge.rs`, `crates/ui-egui/src/embed.rs`, `crates/ui-egui/src/host.rs` | [cronos-bridge.md](cronos-bridge.md) |
| Headless document service | `apps/wordcraft-svc` | [cronos-svc.md](cronos-svc.md) |
| Docker images | `Dockerfile.web`, `Dockerfile.svc`, `docker/` | [cronos-svc.md](cronos-svc.md) |

## Host policy (custom roles)

Roles stay in our backends. For each user the backend computes a policy and the host page sends
it to the editor (bridge `setPolicy`). `Session::run` checks it before every command, so the
ribbon, shortcuts, menus, the control channel and the bridge are all covered. No command can
change it; only the host can (`Session::set_policy`).

```json
{
  "deny": ["review.resolveComment", "review.accept*", "review.reject*"],
  "readOnly": false,
  "allow": [],
  "forceTrackChanges": true
}
```

- `deny`: command ids or prefixes ending in `*`. They are refused with "not permitted for your role".
- `readOnly`: every command that changes the document is refused, except those in `allow` and
  typing inside a comment. A reviewer role is `{"readOnly": true, "allow": ["review.newComment", "review.reply", "review.editComment"]}`.
- `forceTrackChanges`: track changes is switched on and `review.trackChanges` is refused.
- `Session::refusal(id)` tells the UI why a command is refused, so it can grey the button.

## Sections

A section is a body heading and the blocks under it, up to the next heading. With `subtree` it
runs instead to the next heading of the same or a higher level. Text before the first heading is
the *preamble*.

| Command | Params | Result |
|---|---|---|
| `section.list` (pure) | `text?`, `subtree?` | `{"sections":[{"key","level","title","block","end","subtreeEnd","preamble","shown","text"?}]}` |
| `section.select` (pure) | `key` or `block` | caret at the section's start |
| `section.show` (view) | `key` or `block`, `subtree?` | lays out that section alone |
| `section.showAll` (view) | — | the whole document again |
| `section.setKey` | `block`, `key` | puts the key on that heading |
| `section.autoKeys` | — | gives every heading without a unique key one made from its title |
| `section.export` (pure) | `key` or `block`, `subtree?`, `format?` (`docx` default) | `{"data": base64}`: just that section, with its comments |
| `section.replace` | `key` or `block`, then `data` + `name`, or `html`, or `markdown`; `keepHeading?` | the section's blocks replaced; one undo step |
| `review.focusComment` (pure) | `id` | selects the text the comment, or a reply's thread, is on; a section view switches to that section |

**Stable keys (A9).** A key is a hidden bookmark `_Sec_<key>` around the heading's text, so it
survives renaming the heading, saving, and opening the file in Word. A key may contain only
letters, digits and `_`, and at most 35 characters (Word's bookmark-name limit). Other characters
are turned into `_`.

**Section view (A3).** `section.show` sets `Session::section_view`, and the layout then places
only that section's blocks (`LayoutOptions::only_blocks`):

- List numbers, including numbered headings like "5.2", count the blocks that are left out.
  Field results such as SEQ captions are document text, so they are unchanged too.
- Pages that hold none of the section's blocks are dropped. Page numbers in headers and footers
  count only the pages shown, so they don't match the whole document. Exports and prints always
  use the whole document.
- The selection is kept inside the section, so Select All selects the section. After each change
  the engine checks that the blocks before and after the section are unchanged, and refuses the
  change otherwise. This catches Accept All, Replace All and the like.
- The view follows its heading by key, or else by position, when blocks are added before the
  heading inside the section.

**Section replace (A8 groundwork).** The incoming section's styles, lists, pictures, comments and
replies come along, renumbered so they can't collide with ours. Our comments on the replaced text
go. A section break on the replaced text stays, and section breaks in the incoming content are
dropped. If the incoming content starts with a heading, it replaces our heading too, and the key
is kept. Otherwise our heading stays.

## Compare from bytes (A7)

`review.compare` accepts `{"data": base64, "name": "v3.docx"}` for the revised document and
`{"originalData": base64, "originalName": "v2.docx"}` for the original, so the host can compare
two stored versions without touching the file system. Leave out `originalData` to compare
against the open document.

## Known limits (Phase 1)

- Section-level co-editing (A8) has its building blocks (`section.export`, `section.replace`,
  section view and the outside-edit guard), but the lock server and the presence events live in
  our backends (Plan B) and are not built yet.
- Page numbers in the section view count only the pages shown (see above).
- Policies are not enforced for UI-only `ui.*` commands (dialogs, panes). Embedded mode hides the
  ones that matter (File, Backstage).
