<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Host mode

With the `host` cargo feature and `--host`, the relay is itself the sharing
Hachidori. Linked browsers on `/link` get their `hello`, snapshot, `storage`
frames and replies from the relay, never from a browser. A browser that connects
on `/host`, as a sharing Hachidori always does, is only a **source**: the relay
mirrors its configuration and dictionaries and keeps sharing them after that
browser closes, and after the relay restarts.

```sh
cargo build --release --features host
hachidori-relay --host                       # data in the platform data directory
hachidori-relay --host --data-dir ./data     # or here
hachidori-relay --host --network             # also accept linked browsers from the network
hachidori-relay --config relay.host.example.json   # or all of it from a config file
```

A config file (`--config`) sets the same things as `"host": true`, `"data_dir"`
(relative to the file), `"network": true` and `"write_delay"` (seconds), beside
the add-on's `port` and `yomitan_api_port`; a flag on the command line wins over
the file.

Without `--host`, nothing changes: the relay forwards between a browser host and
its clients exactly as upstream does.

**What it does:** mirroring and persistence. Linked browsers receive the
mirrored configuration, and the relay answers their lookups from its own native
engine. The relay's HTTP API (Yomitan's `termEntries`, `kanjiEntries`,
`tokenize`, dictionary downloads, ...) answers too, with no browser connected.
Linked browsers can change the settings the service worker keeps: the dictionary
list, the custom dictionary's source, reader options, lookup counts and word
status. They can also reorder, enable, disable and remove dictionaries, and edit
the custom dictionary, which the relay compiles with its own engine, and import
dictionaries: Settings on a linked browser uploads the archive to the relay. The
relay talks to AnkiConnect itself: it keeps the Anki duplicate index, answers a
linked page's Anki status, duplicate view, word status, maturity and "open in
Anki" requests, Settings' note-type discovery and setup check, the check before
a note is added (preflight), and adding or overwriting the note itself, with its
dictionary images, pronunciation and the linked browser's screenshot. The HTTP
API's `ankiFields` and `ankiCardFormats` are answered from the same rendering.
Managed dictionary updates run on the relay: Settings' checks, installs and
schedule, and the schedule's own runs. So does installing the recommended
dictionaries from Settings or setup.

## Compatible Hachidori

Host mode is compatible with Hachidori
[`d959a09`](https://github.com/bee-san/hachidori/commit/d959a09810cbbcbf8a6113a154a45dfb2f0a88ae)
(Multi-language support, Nov 4 2026). It was ported against
[`8603ae5`](https://github.com/bee-san/hachidori/commit/8603ae554a5597e95e929d11232bacaf5976062a);
the oracle suites (`HACHIDORI_SOURCE`) pass against `d959a09`. Against
`8603ae5` one request differs, and the relay answers as `d959a09` does: the
Yomitan API's `ankiCardFormats` with `profileIndex: null`, which `8603ae5`
rejects and `d959a09` reads as profile 0 (`profileIndex ?? 0`), so
`host-oracle.test.mjs` fails there on it. Its engine is
Hoshidicts `9c4ccdf`, the submodule `d959a09` pins. `8603ae5` pins
`a95758b`; `9c4ccdf` adds the language packs but keeps the Japanese pipeline
identical to `a95758b` (Hoshidicts' `ja-*-parity` tests).

`d959a09` adds language profiles to the sharing protocol (Hachidori's
docs/sharing.md, "Languages"). The relay keeps the single-language form,
which that protocol keeps unchanged for Japanese-only hosts:

- Its hello does not advertise `language-profiles-v1`. A linked Hachidori
  from `d959a09` on therefore does not opt in, and does not forward
  `hd_profile_add` or `hd_profile_remove`; it uses the relay as it uses a
  Japanese-only browser host. Older linked browsers are unaffected.
- A browser host with a second language gives the relay's pull session, which
  does not opt in, its Japanese view, so the relay mirrors its Japanese
  profile.
- A `profileId` in a linked Anki or import request is ignored, so it means the
  Japanese profile, as on a host without profiles. `dictionaryDirections` and
  `dictionaryTargetLanguages` in a mining request are accepted but have no
  effect: they matter only for a profile's notes.
- The Yomitan API's `profileIndex` follows `d959a09`: absent, `null` and `0`
  select the one profile, and `ankiCardFormats` refuses any other index.

## The data directory

The default is `%APPDATA%\hachidori-relay` on Windows, and
`$XDG_DATA_HOME/hachidori-relay` (`~/.local/share/hachidori-relay`) elsewhere.

```text
storage.json                 the shared keys, as plain, pretty-printed JSON
dictionaries/<sha1 of path>/  one dictionary generation's Hoshidicts files
downloads/                   archives built for HTTP dictionary downloads, while open
dictionaries/<sha1>.zip.partial  an upload from a linked browser, until it is imported
```

`storage.json` holds what the extension shares with linked browsers (its
`SHARED_STATE_KEYS`): `dictionaryState`, `options`, `customDictionarySource`,
`dictionaryUpdates`, `lookupStats` and `wordStatusOverrides`, plus the lookup-count
rows (`lookupStats:<generation>:…`). Values are stored exactly as the extension
stores them. Each write replaces the whole file through a temporary file and a
rename, so a crash leaves either the old or the new file.

The file is written behind. A change is in memory, and sent to linked browsers,
at once; the file is rewritten at most once per `--write-delay` (5 seconds by
default), with every change made in that time. A linked browser records a
lookup count for each lookup, so without the delay each lookup would rewrite the
whole file. Two kinds of write do not wait: a change of `dictionaryState`, which
is on disk before the dictionaries it replaces are deleted, and the last write
when the relay stops (Ctrl-C, SIGTERM, or on Windows closing the console window,
logging off or shutting down). A crash, or a process that is killed, loses at most
the last delay's changes. `--write-delay 0` writes every change before it is
applied, as an older relay did.

A dictionary is installed per **generation**: the `path` of its entry in
`dictionaryState`. The extension gives a dictionary a new generation path every
time it imports it again, so an installed generation never changes. Its
directory is named by the SHA-1 of that path. Generations that the stored
`dictionaryState` no longer names are deleted. Interrupted downloads and
unpacks (`*.partial`) are deleted on the next start.

A dictionary download from the relay is the archive the extension's
`hd_api_dictionary_open` builds: the installed files, and a manifest made from
what is stored now (the dictionary's entry and groups, the reader options, the
custom source for the custom dictionary, the update settings and the lookup-count
descriptor). An older relay kept a `.hachidori-relay-manifest.json` next to each
generation; it is no longer used, and never served.

## Pulling from a browser host

While a browser host is connected, the relay keeps one in-process client
session open to it. That session has the same origin (`relay://yomitan-api`) and
capability (`hoshidicts-api-v1`) as the API session, so the browser does not
list it as a linked browser.

1. The browser's `hello` snapshot overwrites the six stored keys. A key missing
   from the snapshot is removed. The browser's values always win.
2. Every `storage` broadcast from the browser is applied the same way. Keys that
   are not shared (for example `sharing`) are ignored.
3. Every stored change is broadcast at once to the linked browsers as a
   `storage` frame, in the order it was stored.
4. A changed `dictionaryState` is held back. For each generation it names that
   is not installed yet, the relay downloads the dictionary with the host
   contract's `hd_api_dictionary_open` / `read` / `close` frames
   (`docs/host-contract.md`). The download is the extension's one-dictionary
   backup archive. The relay checks each entry's CRC32 and unpacks
   `dictionaries/0/` into the generation's directory. Only when every generation
   is present is the new `dictionaryState` stored and published, so a linked
   browser never sees a dictionary the relay cannot read. Then the generations
   that are no longer needed are removed.
5. If a download fails, nothing is published, and the previous list stays in
   force. The relay tries again after 30 s, or at once when a newer
   `dictionaryState` arrives.

The browser host cannot reach the relay's linked browsers. Its `send`, `close`
and `broadcast` frames go only to its own clients (the pull session). Its
`network` frames are answered with how the relay listens now, without switching.
Losing the browser host closes only its own clients.

## Linked browsers

The relay answers as the extension's `sharing-host.js` does:

- `hello` (protocol 1) gets `{kind: "hello", protocol: 1, version:
  "hachidori-relay/<version>", name: "Hachidori Relay", dictionaryCount,
  capabilities: ["hoshidicts-api-v1", "linked-import-v1", "linked-anki-v2"], snapshot}`. The snapshot is the six keys,
  `null` for any that were never set.
- Another protocol, or a frame that cannot be read, gets `bye` with the
  extension's reason, and the connection is closed.
- `request` gets a `reply` with the same `id`, routed as the extension's
  `dispatchSharedRequest` routes it. An unknown target, or a type the target does
  not forward, fails as the extension fails it: `{type: "<type>_result",
  requestId, ok: false, error, generation: 0}`.
- The engine's requests to `hoshidicts-offscreen` are answered by the relay:
  `hd_lookup`, `hd_lookup_dictionary`, `hd_segment`, `hd_kanji`, `hd_styles`,
  `hd_media`, `hd_status`, `hd_memory` and `hd_memory_total`. They get the
  engine service's envelope (`{type: "<type>_result", requestId, ok, error,
  generation, ...}`) with its argument checks, defaults and response limits.
  What `engine-service.js` adds to the engine's replies is ported as well:
  definition tags expanded from the tag banks and sorted as `Intl.Collator`
  sorts them, `term.furigana`, and the personal-dictionary filter.
- The host side of the Yomitan API (`api-host.js`): `hd_api_version`,
  `hd_api_term_entries`, `hd_api_kanji_entries`, `hd_api_tokenize`,
  `hd_api_dictionaries` and `hd_api_dictionary_open` / `read` / `close`. As in
  the extension, these answers are bare results, not engine envelopes. The relay's
  own HTTP API reaches them through its in-process API session, so it works
  without any browser connected.
- The service worker's storage requests to `hoshidicts-worker`, as its
  `WORKER_HANDLERS` answer them: `hd_state_read` / `hd_state_cas`,
  `hd_custom_read` / `hd_custom_cas`, `hd_options_write`,
  `hd_lookup_stats_read` / `hd_lookup_stats_record` and
  `hd_word_status_override`. They run one at a time, with the extension's
  checks, revisions, conflict and refusal replies, and its commit rules: a new
  dictionary list prunes group memberships and moves the reader options'
  dictionary selections along with renamed or removed dictionaries; the managed
  custom dictionary stays first and enabled and changes only with its source;
  lookup counts start a new random generation. Every write is stored and then
  broadcast to every linked browser as a `storage` frame. A linked browser whose
  hello does not carry `linked-anki-v2` gets the extension's compatibility rules:
  its Custom Links edit the matching Custom Buttons, its legacy Anki fields edit
  the first Template, and editing Templates or Custom Buttons themselves is
  refused with "Update the linked Hachidori before editing Templates or Custom
  Buttons."
- The engine's requests that change the dictionaries, as `engine-service.js`
  answers them: `hd_apply_state` (a new order, or dictionaries enabled and
  disabled), `hd_remove`, `hd_reload`, and `hd_custom_save` /
  `hd_custom_append`. As in the extension, the engine reads and commits through
  the worker requests above (`hd_state_cas`, `hd_custom_cas`, ...), here in
  process and in the same queue. It loads what it is about to commit first, so a
  package that will not load is never committed, and it rolls back to the
  committed list when a commit is refused. A custom source with entries is
  compiled into a Yomitan archive (`buildCustomDictionaryZip`), imported with
  the native `hdw_import` into a new generation and committed with the source.
  A source whose entries are unchanged is saved alone; an empty one removes the
  package. A generation that a removal or a new custom package leaves
  unreferenced is deleted.
- Dictionary uploads to `hachidori-linked-import` (`linked-import-v1`), as the
  extension's `linked-import.js` hosts them: `hd_import_begin` (file name, size
  and whether to replace), base64 `hd_import_chunk`s of up to 1 MiB in order, and
  `hd_import_commit` or `hd_import_abort`. Each request is rebuilt from the
  fields it needs (`allowLinkedImportRequest`). An upload belongs to the linked
  browser that began it; it is dropped when that browser leaves, after a chunk out
  of order or that does not fit, and two minutes after its last chunk. The bytes
  wait in a file in the data directory. The commit reads the archive's
  `index.json` and decides as the extension decides for a remote sender
  (`uploadImportDecision`): a new title installs; an installed one, matched by
  title or else by update source, is replaced (keeping its name, order, groups and
  update source) or kept beside the new copy as `Title (2)`. It then runs the
  engine's `hd_import` for that decision (`commitImportedGeneration`): the
  archive is compiled with the native `hdw_import` into a new generation, loaded
  with the rest of the list, committed, and the generation it replaced deleted.
  Any failure loads the committed list again and deletes the new generation. The
  commit's reply is the engine's import reply, report included.
- Anki (`target: "hachidori-anki"`, and the worker's `hd_anki_discover` and
  `hd_anki_setup`) is answered by the relay over AnkiConnect, at the URL and API
  key in `options.anki`. Requests keep only the fields the extension keeps
  (`allowLinkedAnkiRequest`), and configuration keys carry a prefix new at each
  relay start (`linked:<uuid>:`), so a key from another host is refused.
  - `hd_anki_status`, `hd_anki_view`, `hd_anki_maturity` and
    `hd_anki_word_status` read the duplicate index, which the relay keeps in
    `ankiDuplicateIndex` as the extension does (`anki-index-cache.js`): pulled at
    start, when `options` change, and every 30 minutes, from the selected note
    type's notes. Each change of its rows or source reaches linked browsers as a
    storage frame and a `word-status` frame.
  - `hd_anki_browse` repairs a stale duplicate (looks the word up again) and
    opens Anki's browser on its notes (`guiBrowse`).
  - `hd_anki_discover` and `hd_anki_setup` read decks, note types and fields,
    and detect or verify the Template the way Settings' setup check does.
  - `hd_anki_preflight` checks the dictionary result against the engine's
    current generation, renders the Template's fields the way the extension's
    offscreen document does (`buildAnkiResourceFields`: every marker, the
    glossary with its structured content and dictionary styles, Smaller Anki
    cards' compact glossary, pitch graphs, frequencies), refuses an empty first
    field, and decides the duplicate behaviour from the index and Anki's own add
    check (`canAddNotesWithErrorDetail`, falling back to `canAddNotes`).
    A first field with `{audio}` downloads the pronunciation first, since it is
    part of the note's identity.
  - `hd_anki_submit` writes the note as the extension's `write` does, one
    submission at a time: it decides again against a fresh configuration, stores
    the screenshot the linked browser sent with the request (its client-media
    envelope, validated as `validateLinkedAnkiClientMedia` does), the dictionary
    images the fields show and a first-field pronunciation, then adds the note
    (`addNote`) or overwrites the duplicate's fields by each Template's
    overwrite mode (`updateNoteFields`). It records the note in the duplicate
    index, reads the note back and reports fields Anki saved differently, and
    then adds the pronunciation to the fields that want it. A refused write
    deletes the screenshot it stored; a write whose answer was lost is reported
    as uncertain, never retried.
  - Pronunciations come from the enabled audio sources (an audio URL, or a
    Yomitan JSON list), downloaded by the relay itself.
- Managed dictionary updates (`hachidori-updates`) are the extension's
  `background-updates.js`. `hd_updates_schedule` writes the schedule
  (`dictionaryUpdates`) against its revision. `hd_updates_check` reads the
  update index of each managed dictionary (one whose recommended catalogue entry
  or own `index.json` names an HTTPS update source) and records what it found
  on the dictionary (`lastUpdateCheck`). `hd_updates_install` also downloads
  each newer archive and installs it through `hd_import`. Cycles run one at a
  time, and a dictionary that changed meanwhile is reported, not updated.
- `hd_import` from an `archiveUrl` is the extension's remote import: a checked
  managed update (its fingerprint, expected revision and check time), or a first
  install of a recommended source from its catalogue archive. The request is
  checked, the archive downloaded with nothing locked, and the request checked
  again before the archive is compiled; the redirects must end at an HTTPS URL,
  for a recommended source at its own release asset. The new package replaces
  the one it updates, keeping its name, order, groups and source, and records
  the check as up to date. Its revision and update source must be the ones the
  index named.
- Installing the recommended dictionaries (`hachidori-setup`) is the
  extension's `setup-installer.js`. `hd_setup_install` attaches to the active
  run, or starts one for the catalogue sources it names; an empty request only
  observes, which is how linked pages follow the run. A run installs its
  sources one at a time, in catalogue order, through the remote import above,
  and never one that is already installed (by its catalogue source or its exact
  update index); it waits while another import runs. Each outcome is recorded
  as the extension records a shared request's (`hd_setup_record` without
  startup's record): a source's first install selects it once for its
  dictionary option (Jitendex for the compact definition summary, Bee's
  Ultimate Kanji Dictionary for kanji clicks) while that option is still
  Automatic, and `recommendedDictionarySelections` remembers it.

The rules are ported from the extension's JavaScript to Rust (`src/host/settings`).
`tests/conformance/settings-oracle.test.mjs` boots the extension's service worker
with its own smoke-test fakes and the same stored settings as a relay, sends each
request of a scripted sequence (commits, conflicts, refusals and errors, from a
current and a legacy client) to both, and requires the same reply and the same
shared storage after every step. `tests/conformance/anki-oracle.test.mjs` does the
same for Anki: both sides use one fake AnkiConnect (`anki-fake.mjs`, a small
collection with Anki's search syntax and its add check), and each reply, the
stored duplicate index, the queries sent to Anki's browser and the notes sent to
Anki's add check (the rendered fields) must agree. Each submission runs on both
sides from the same collection, and the notes and media files each leaves in it
must agree too; the fake also serves the pronunciations. The extension's side
renders in jsdom with its own `buildAnkiResourceFields` and downloads with its
own `exportAnkiAudio`, with a stand-in for the browser's audio decoding. The option normalisation and the custom
dictionary's parser and semantic revision are also checked against 4,321 cases
recorded from the extension's code (`scripts/settings-cases.mjs`).

## The engine

The relay links Hoshidicts (`third_party/hoshidicts`, the extension's engine)
natively, built by cmake with the extension's `wasm/bindings.cpp` API. It loads
the enabled dictionaries of the stored `dictionaryState` in order, every kind
each package has (terms, frequencies, pitches, kanji), and reloads when that list
changes. A package that fails to load is skipped, and its error is listed in
`hd_status`. The engine runs on blocking threads, one request at a time.

`tests/conformance/host-oracle.test.mjs` checks this against the real extension.
It boots the extension's engine service on its own wasm build, imports its test
dictionaries, mirrors them into the relay, and requires the same reply from both
for every request it sends. `tests/conformance/mutations-oracle.test.mjs` does
the same for the mutations: it builds, appends to, re-saves and empties the custom
dictionary, reorders, disables and removes dictionaries, and sends refused and
invalid requests, and requires the same replies, the same stored dictionary state
and custom source after every step, and the same lookups over the result.
`tests/conformance/import-oracle.test.mjs` uploads the same archives, with the
extension's own `uploadDictionary`, to the extension's upload host and to the
relay: new, kept beside, replacing a dictionary and a mirrored fixture, archives
that do not import, and refused and broken uploads. It requires the same replies,
storage and lookups, and the same download archive of what was imported. Furigana are also checked against 6,035 cases
recorded from the extension's JavaScript (`scripts/furigana-cases.mjs`).

Managed updates have no oracle test (that needs an HTTPS server the relay
trusts). With `HACHIDORI_LIVE_TESTS=1`, `tests/conformance/updates-live.test.mjs`
runs them against the real releases of Bee's Ultimate Grammar Dictionary: an
older release (2026.10.07.3), uploaded as a linked browser would, is checked,
updated to the live revision through its own update index and checked again;
a first install from the recommended catalogue downloads the latest release
asset and records its catalogue source; and the recommended-dictionary
installer installs it, reports its run to an observing request, and finds it
already installed on a second run.

## Differences from the extension

- The relay has one profile, Japanese: no language profiles, and no
  `language-profiles-v1` (see [Compatible Hachidori](#compatible-hachidori)).
- Storage is written behind, at most once per write delay (see
  [The data directory](#the-data-directory)); the extension's storage writes
  each change. A crash loses at most the last delay's changes, never a
  dictionary list.
- A note's rich definition fields share one random style prefix (the renamed
  dictionary custom properties); the extension draws one per field. Every field
  on a card sees every field's styles anyway, and a dictionary's styles are the
  same in each, so cards look the same; each dictionary stylesheet is then scoped
  and parsed once per note instead of once per field.
- The hello's `name` is `Hachidori Relay` and its `version` is
  `hachidori-relay/<version>`. It advertises `hoshidicts-api-v1`,
  `linked-import-v1` and `linked-anki-v2`, but not the legacy
  `linked-anki-v1`: a linked browser too old for host-owned mining is told to
  update.
- The recommended-dictionary installer reports no download progress and no
  installing phase: an entry goes from `downloading` to its outcome, with
  `receivedBytes` 0 and `seconds` 0 for an install. Linked pages learn of
  progress by asking, every four seconds, as they already do; there are no
  `hd_setup_progress` events. Startup's onboarding record (`setupState`) is
  never written, as the extension does not write it for a shared request.
- The update schedule is looked at once a minute rather than by a browser
  alarm, and not while a browser host is connected at `/host`: that browser
  runs its own schedule, and the relay mirrors its dictionaries.
- Download progress is not reported (the extension reports it only to its
  installer). An update index is read for at most two minutes and 16 MiB; the
  extension's limit is two minutes without data.
- A failed download says "Failed to fetch", as Chrome's `fetch` does; the
  network error itself is in the relay's debug log.
- `generation` in engine replies counts the relay's own reloads. It is not the
  browser's number, so media requests must use the generation the relay sent.
- `hd_status` describes the native engine: `storageBackend: "native"`,
  `threaded: false`, `hashIndexStorage: "resident"`, `residentHashBudgetBytes:
  null`. There is no wasm heap.
- `hd_memory`'s `heapBytes` (and `hd_memory_total`'s `bytes`) is the size of the
  loaded index files plus the engine's page cache, not a wasm heap size.
- Only enabled dictionaries are loaded. Like the extension, a mutation checks
  that a disabled package it introduces loads; unlike it, a reload does not check
  the disabled packages already committed.
- `hd_apply_state`'s `loadPath` is always `"full"`. The relay always loads
  the whole list again; the extension also reorders in place (`"order-only"`) or
  adds and drops single packages (`"incremental"`).
- `hd_reload` loads the stored `dictionaryState` again. The extension's
  `reconcile` also adopts packages it finds on disk that no list names and
  migrates a legacy dictionary list; the relay has neither.
- After a mutation the relay deletes only the generations that mutation left
  unreferenced. The extension deletes every unreferenced one that no automatic
  backup keeps; the relay keeps no automatic backups, and removes the others when
  it next mirrors a browser host's list.
- An import runs with the native importer's ordinary memory use, once. The
  extension's Low memory mode and its reduced-memory retry after a memory failure
  are not needed natively and are not ported.
- An upload is imported while the relay's storage queue is held, so the service
  worker requests of other linked browsers wait for the import to finish. The
  extension imports outside its storage queue.
- An upload is kept in a file, not in memory, and is not limited in size beyond
  the disk.
- An upload that is not a ZIP archive, or whose `index.json` cannot be read, is
  refused with the relay's own wording: the extension's comes from zip.js.
- While a browser host is connected, its next `dictionaryState` replaces the
  relay's, so a custom package or removal made through the relay lasts only until
  the browser changes its own list.
- `hd_api_version` (the HTTP API's `yomitanVersion`) reports the relay's own
  version.
- A dictionary download's manifest does not go through the extension's
  `assertBackupSnapshot` before it is served.
- `hd_api_anki_fields` returns its fields object in alphabetical order of
  the markers, not the order they were asked for; JSON readers do not see the
  difference.
- The relay has no browser speech. Text-to-speech audio sources are left out
  of its options, as a host without speech recording leaves them out, so
  pronunciations come only from audio URLs and Yomitan JSON lists.
- The extension decodes a downloaded recording in an `Audio` element to be
  sure it plays. The relay cannot decode audio, so it accepts a recording whose
  first bytes are those of MP3, AAC, WAV, Ogg, FLAC, WebM or MP4 audio, and
  refuses anything else as the extension refuses what it cannot decode. It
  downloads at most 32 MiB of a recording; the extension has no such limit.
- The extension renders fields in a browser document; the relay has its own
  small HTML tree and CSS reader (`src/host/anki/dom.rs`, `css.rs`). So:
  - Dictionary styles are scoped and written out as the dictionary wrote them,
    not re-serialised the way the browser's CSSOM would (which normalises
    whitespace, colours and shorthands).
  - Inline style values from structured content are kept as written, not
    normalised by the browser.
  - Smaller Anki cards' compact glossary reads a cascade of the browser's
    default styles, the dictionary's styles (honouring `@supports`, ignoring
    `@media` and `@container`) and inline styles. It has no layout, so nothing
    that depends on the viewport changes what it keeps.
  - Structured content nested more than 4,096 levels deep is refused.
- A request missing one of a dictionary result's lists (`trace`, a pitch's
  `nasal`, and so on) is read as if it were empty; the extension fails with a
  JavaScript error.
- A configuration key's digest hashes the Template as the relay serialises it,
  so it differs from the extension's for the same Template. Keys are only
  compared with the relay's own, so this is not visible to linked pages.
- The extension pulls the index in its offscreen document and suspends the
  refresh while it is not needed; the relay runs it on its own threads, and
  always.
- Each pull asks AnkiConnect anew: the relay does not share a running live
  lookup between identical requests, as the extension's offscreen page does.
- While a browser host is connected it stays the source: what linked browsers
  write to the relay is not sent back to it, and the browser's next change of a
  key replaces what they wrote there.
- Lookup-count rows are mirrored only as the browser host changes them. The
  sharing protocol has no way to read them all at once, and the snapshot does not
  carry them.
- A dictionary's new `dictionaryState` reaches linked browsers only once its
  files have been downloaded, not at the moment the browser stored it.
