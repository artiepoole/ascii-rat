# Writing instructions

These conventions apply to Markdown documentation in this repository (the
`README.md` and any other docs). Follow them when adding or editing prose.

## Bold text

**Do not use bold text for emphasis.** Emphasising words or phrases in running
prose (for example "the password is **never** stored") is not allowed. Write the
sentence plainly and let the wording carry the meaning.

Bold (`**...**`) is reserved for two purposes only:

- **Table headings** — the heading row / heading cells of a Markdown table.
- **Inline headings** — a short leading term that labels the item it introduces,
  such as the first word(s) of a list item or the leading term inside a table
  cell (for example a list of action forms where each item starts with the form
  name in bold).

If a bit of text is not a table heading and is not an inline heading, it must not
be bold.

## Rationale

Using bold only for headings keeps documents scannable: bold reliably signals
"this is a label", not "this is important". Overusing bold for emphasis makes the
real headings harder to spot and clutters the text.

# Committing

Create a separate commit for each change. A "change" is one self-contained,
logically distinct piece of work (a feature, a fix, a documentation update).

Each commit must only ever include the files that belong to that change.
Never stage or commit unrelated files just because they happen to be modified in
the working tree. In practice this means committing named files explicitly (for
example `git commit <file>...` or `git add <file>...` followed by `git commit`)
rather than sweeping everything in with `git commit -a` or `git add .`.

If you have made several unrelated changes at once, split them into one commit
per change, each staging only its own files.
