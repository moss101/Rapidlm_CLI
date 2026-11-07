# File references and completion keys

## `@path` references

Naming a file in a prompt with `@` attaches it to the turn, so the model has it without spending a tool call
to find it.

| You type | The model is given |
|---|---|
| `@src/lib.rs` | the file |
| `@src/lib.rs:40-80` or `@src/lib.rs#L40-L80` | lines 40 to 80 of it |
| `@"notes/a b.txt"` | a path with spaces |
| `@src/` | a listing of the directory |

## Images

An image named with `@` is sent to the model as an image, beside the text, not read as text:

```
what is wrong in @screenshots/login.png ?
```

PNG, JPEG, GIF and WebP are recognised by their first bytes, not their names: a `.png` that is really text is text, and
a photo called `notes.txt` is a photo. At most 4 images and 3 MiB each go with one prompt. They are held out of the text
context, so their base64 is never counted or billed as text.

**The model must be marked as able to see.** Set `vision = true` in its `[model.<id>]` table (see
[model configuration](model-configuration.md)). Capabilities are never guessed upward, so without it the images are
not sent, and RapidLM says so on the spot (`1 image(s) were not sent: this model is not marked as able to see them`).

## Video

A trusted workspace can attach one `.mp4`, `.m4v`, `.mov`, or `.webm` file per prompt using `@path`.
Set `video_input = true` for a video-capable `google-genai` or `vertex-genai` model. Each model in a fallback chain must support and enable video. RapidLM checks the file header and sends up to 3 MiB inline. For `google-genai`, larger files up to 100 MiB are uploaded to the provider, referenced during the model step, and deleted afterward; `vertex-genai` currently accepts inline video only. Each fallback must support the attachment's size. Invalid, missing, unsupported, or oversized video stops the turn. Line ranges do not apply to video.

Large uploads require a writable RapidLM home directory for a private upload journal. RapidLM waits up to 30 seconds for processing before the model step. If an upload or deletion has an uncertain outcome, a journal record is retained in `video-uploads` under RapidLM home. On a later large-video request using the same configured API key, RapidLM attempts to clean up at most two finalized records after verifying their remote metadata. Records for changed keys, incomplete or uncertain uploads, unavailable files, and older records without a key binding remain for inspection. A provider file may remain after an interrupted operation, and file lifecycle behavior has not yet been verified with a live Google account.

When a turn pauses for an approval or answer, the live host retains the original video bytes for its continuation, even if the file changes. The bounded cache can evict an older pending attachment. After eviction or a host restart, RapidLM rereads the workspace file and continues only if its type, path, and content digest match the original attachment. If they do not match or the file is unavailable, the continuation stops and asks for a new prompt with the video.

**Dropping a file onto the terminal** pastes its absolute path, with spaces escaped by a backslash. Type `@` before it:
`@/Users/me/proj/My\ Shots/Screen\ Shot.png` works, as does `@"My Shots/Screen Shot.png"`. An absolute path is honoured
only when, with links resolved, it lies inside the project, the same test a relative path faces. A file elsewhere on
disk is refused: copy it into the project first.

## Model file and search tools

`workspace_read` recognizes PNG, JPEG, GIF and WebP by their bytes. Static PNG, JPEG
and WebP images can be cropped with `region: {x, y, width, height}` in oriented
original-image pixels. The optional `max_edge` (1–8192, default 2000) and
`byte_budget` (up to 3 MiB) control transformed output. A static image that exceeds
the default edge or output budget is resized or recompressed automatically; inputs
are limited to 8 MiB. `full_resolution: true` prevents resizing and fails if the
encoded result exceeds the budget; JPEG output may still be recompressed. Animated
PNG and WebP cannot be transformed, and GIF remains available as an unchanged
payload within 3 MiB. PDF extraction returns bounded text. Images are omitted from
a request when the selected model has not been configured with `vision = true`.

Text reads recognize UTF-8, an optional UTF-8 BOM, and UTF-16 with a little- or
big-endian BOM. Malformed sequences are replaced and each affected result carries a
warning. File-size limits apply to the original bytes before decoding.
UTF-16 is transcoded only for display: patch operations require UTF-8, and full
writes produce UTF-8, so convert the file deliberately before editing it.

Before a model tool replaces an existing file, the model must have received a
successful read of its current bytes in an earlier model step. If the file changes,
or after a successful write, read it again before another replacement. Creating a
new file does not require a prior read. A read and write proposed in the same batch
do not satisfy this requirement. An approval for an edit can carry its turn's
read authority while the interactive session remains running, but the file is
checked again when the approval is applied. A new turn or a resumed process starts
without earlier read authority and may need to re-read.

`repo_read` accepts `path`, `offset` (one-based; negative values count from the end),
`limit` (1–1000 lines), `max_bytes` (4–32768; default 4096), and `column_offset`
(zero-based UTF-8 bytes in the decoded first line). Pages prefer whole lines. When one line
is too wide, the result contains `Next Read` JSON to continue **that same line**. Join
those fragments without an added newline; a column must be a UTF-8 character boundary.
Column offsets cannot be combined with a negative line offset. Each call reads the
current file; restart a read if the file changes.

`repo_search` accepts `pattern`, `regex` and `ignore_case` (both default false), an optional
workspace-relative `path`, an optional `glob`, `head_limit` (1–100; default 20), and
`offset` (default 0). It uses the Context Fabric's ignore-aware search: `.gitignore`,
`.rapidlmignore`, build/vendor directories and hidden paths are excluded. Every matching
line is returned in path/line order. Follow `Next offset` when a page is cut by its line
or byte bound. A two-second timeout or the 10,000-hit ceiling is reported explicitly;
narrow the path or glob. Files above the context engine's 1 MiB text eligibility limit
are not searched as text; a selected oversized file or a traversal omission marks
the result incomplete. Binary files and intentionally ignored paths are excluded.
Search results describe the filesystem at each invocation.
Long matching lines are marked when abbreviated; use `repo_read` at the reported path
and line to retrieve the full content in bounded pages.
`output_mode` may be `content` (default), `files` (distinct matching paths), or
`count` (matching-line count per file). Pagination counts the displayed rows. Counts
and file listings are incomplete if the hit ceiling is reached.
In content mode, `context` requests up to 20 lines on both sides of each hit;
`before_context` and `after_context` independently override that value. Overlapping
or adjacent ranges merge, and pagination counts the resulting path/line rows.
Matching rows use `path:line:`, while surrounding rows use `path-line-`. The
file is checked against the searched bytes before its context is returned; if it
changed, retry the search. Long context rows are marked for `repo_read` as well.

`repo_glob` accepts `pattern`, `head_limit` (up to 100), and `offset`. It applies the
same ignore rules and hidden/build-directory exclusions as search. Paths are ordered
and pages report `Next offset`; a timeout or 10,000-match ceiling is reported instead
of presenting an incomplete walk as complete. A traversal bound or an entry that
could not be examined also marks the result incomplete, even when no paths matched.

## Prompt text references

The prompt you typed is what the ledger records; the attached text travels as separate context, in the
read-set partition at a score above retrieved snippets, so under context pressure it is the last thing
dropped. RapidLM prints a line saying what was attached and what was not.

References are honoured only in a trusted project, and only within these limits:

- The path must stay inside the project once links are resolved. `@../x`, an absolute path, and a link
  leading outside are refused. `.git` is refused.
- A file that looks like it holds a secret (the same scanner that checks files the agent writes) is not
  attached. The whole of what was read is scanned, not only the lines attached, so a range cannot step below a
  private key's header. A file that cannot be scanned is not attached. A finding dismissed with
  `rapid findings dismiss` does not count.
- A credentials file is refused by name: `.env` and `.env.*`, `.netrc`, `.npmrc`, `.pypirc`, `credentials`,
  `secrets.json/yml/yaml`, `id_rsa*` and the other key names, and `.pem .key .p12 .pfx .keystore .jks .kdbx`.
  Names ending `.example`, `.sample`, `.template`, `.dist` or `.defaults` are allowed. Ask the agent to read the file
  if you mean to share it.
- Binary files are refused. A text file is cut at 64 KiB at a line boundary, and the cut is stated; one
  prompt attaches at most 192 KiB and 16 references. A line range is found anywhere in the first 8 MiB of the file
  and is itself cut at 64 KiB.
- `@alice` in running text, and an e-mail address, are not references. A reference to something that is
  not a file is reported only if it looks like a path (it contains `/` or `.`).

## Keys

| Key | Does |
|---|---|
| Tab | with an `@path` at the end of the prompt, completes it; with `/name`, completes the command; with an empty prompt, switches between plan and build |
| Shift-Tab | switches between plan and build, whatever the prompt holds |

Completion takes a unique match whole. With several, it extends to their shared prefix and lists them. A
name beginning with what you typed beats a looser match; when nothing begins with it, files are ranked by
how well the path fits what you typed as a subsequence, with a bonus where letters start a path segment or
word (`@kcl` finds `crates/kernel/src/client.rs`). A name with a space is quoted: a directory leaves the quote open
so you can keep typing (`@"my notes/`), a file closes it. `@packages/@scope/x` completes although it holds an `@`.
In a git checkout, ignored files are left out; the listing never runs the repository's own `core.fsmonitor`
program, is limited to three seconds, and is reused for two. Command completion includes your own commands (see
[custom commands](custom-commands.md)).

Plan mode is the session's `/plan`: the model may read and may write only its plan. It is switched between turns,
not during one.
