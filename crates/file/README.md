# airsplice-file

Reads and writes `.airsplice` files: YAML documents that AirSplice tools
share. airsplice-player saves its playlists in them.

```yaml
version: 1
player:
  playlist:
    dissolve: 1
    entries:
    - intro.mp4
    - path: slides/title.png
      dissolve: 2.5
    - path: /media/talk.mp4
      dissolve: 0
```

## Versions

`version` is the format version, a whole number from 1, and every file
has it. It changes only when an existing key changes meaning or shape;
adding an optional key does not change it. A reader refuses a file with a
newer version than it knows, and reports that before looking at anything
else in the file. A writer writes the newest version it knows.

## Sections

Each tool keeps its data under one top-level key named after the tool:
`player` for airsplice-player. A file can hold several. A tool that
rewrites a file keeps every key it does not know, at the top level and
inside sections and entries, so tools can add their own data without
breaking each other. To add data to another tool's section or entries,
use a key named after your tool.

## `player`

| Key | Meaning |
|---|---|
| `playlist` | the playlist |

### `player.playlist`

| Key | Meaning |
|---|---|
| `entries` | the entries, in play order |
| `dissolve` | seconds each entry dissolves in over, unless it gives its own |

An entry is a path, or a mapping:

| Key | Meaning |
|---|---|
| `path` | the file; required |
| `dissolve` | seconds this entry dissolves in over; 0 cuts to it |

Without a `dissolve` on the entry or the playlist, the player's own
setting applies. Seconds are numbers, 0 or more.

Paths are relative to the file's folder, or absolute. Relative paths use
`/` on every system.

## Use

```rust
let doc = airsplice_file::Document::read(path)?;
if let Some(playlist) = doc.player.as_ref().and_then(|p| p.playlist.as_ref()) {
    let folder = path.parent().unwrap();
    for entry in &playlist.entries {
        println!("{} {:?}", entry.resolve(folder).display(), playlist.dissolve_into(entry));
    }
}
doc.write(path)?; // what the reader did not know is kept
```
