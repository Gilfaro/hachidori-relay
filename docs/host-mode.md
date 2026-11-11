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

Host mode is compatible with Hachidori 0.5.1,
[`d41c37a`](https://github.com/bee-san/hachidori/commit/d41c37abe4ea54ef6ec885e9517bbf5655f3d718)
(Nov 10 2026), language profiles included: a 0.5.1 browser can link to the
relay, share every language pack the extension ships, mine and look up per
profile, and use the HTTP API per profile, as with a browser host. Its engine
is Hoshidicts `088ac5d`, the submodule `d41c37a` pins. What the relay does
differently is listed under
[Differences from the extension](#differences-from-the-extension).

The oracle suites run the extension's own code from a checkout of `d41c37a`
(`HACHIDORI_SOURCE`) against the relay and pass:

| Suite (`tests/conformance/`) | What the extension's code checks |
| --- | --- |
| `host-oracle` | engine requests, lookups, kanji, media, status |
| `import-oracle` | archive and upload imports, per profile |
| `mutations-oracle` | dictionary mutations, custom dictionaries, profile add and remove |
| `settings-oracle` | the settings worker: options, counts, marks, schema 2 records |
| `anki-oracle` | preflight, Add, browse, statuses, audio and rendered fields |
| `profiles-oracle` | lookups, Anki and the Yomitan API in each profile |
| `sharing-oracle` | the extension's sharing host against the relay's, language opt-in |
| `anki-index`, `anki-index-languages` | the duplicate index's pulls, repairs and sources per profile |
| `host`, `api`, `sharing-relay` | the protocol, the HTTP API and the pull from a browser host |

Recorded from it: `src/host/settings/testdata/options.json` (options
normalisation) and `src/host/anki/testdata/languages.json` (Anki fields in
each language, `scripts/anki-language-cases.mjs`).

Each of `d41c37a`'s `test/` files that tests host behaviour has a relay
counterpart:

| Upstream `test/` | Relay |
| --- | --- |
| `sharing-protocol`, `sharing-host`, `sharing-profiles`, `capabilities`, `message-types` | `host.test.mjs`, `sharing-oracle`, `src/host/local.rs`, `sharing_languages.rs` |
| `profiles`, `language-packs`, `language-library`, `language-scripts`, `engine-profile-code` | `profiles-oracle`, `src/host/settings/profiles.rs`, `src/host/profile.rs`, `setup.rs` |
| `dictionary-import*`, `dictionary-index-storage`, `linked-import`, `frequency-direction`, `custom-dictionary` | `import-oracle`, `mutations-oracle`, `src/host/upload.rs`, `src/host/engine/` |
| `recommended-*`, `setup-installer`, `setup-state`, `dictionary-update-schedule`, `update-check-timeout` | `src/host/setup.rs`, `remote.rs`, `updates.rs`, `updates-live.test.mjs` |
| `anki*` (but `anki-addon`, `anki-offscreen-audio`), `kanji-group-mining`, `word-status-overrides`, `sentence`, `furigana` | `anki-oracle`, `anki-index*`, `src/host/anki/` |
| `audio-repository`, `audio-sources`, `audio-languages`, `audio-content` | `src/host/anki/audio.rs`, `anki-oracle` |
| `api-host`, `yomitan-parity`, `search-*` lookups | `api.test.mjs`, `profiles-oracle`, `src/host/yomitan.rs` |
| `lookup-stats`, `reader-options`, `advanced-settings`, `experimental-*`, `low-memory-option` | `settings-oracle`, `src/host/settings/` |

The rest test the extension's pages, content scripts and Settings (the popup,
themes, Netflix, YouTube, Crunchyroll, user scripts, keybinds, backups), which
the relay does not have.

Behaviour 0.5.0 adds beyond the protocol, which the relay follows:

- accepts the options 0.5.0 writes: `glossaryLayoutMode`
  `"compact-popup-anki"` and the experimental `multipleLanguages` switch (a
  0.5.0 browser's settings write was refused before);
- stores an imported frequency list that declares `occurrence-based` but holds
  ranks as `rank-based` (Hachidori #684, `src/host/engine/frequency.rs`);
- repairs any term the duplicate index can answer for when Anki's browser is
  opened, not only one the request named notes for (#683);
- segments without decompressing glossaries or reading pitches, as 0.5.0's
  `hdw_segment` does (Hoshidicts #84);
- mines in a single request where Anki's own duplicate check is enough (#683):
  with duplicate scope `model`, no overwrite and a first field of exactly
  `{expression}`, preflight sends only `canAddNotesWithErrorDetail` and renders
  only the first field and authored cloze fields, and Add is one `addNote` that
  carries the screenshot and pronunciation as native attachments (media inside
  rich HTML is stored first, in one `multi`), with no readback or enrichment;
- keeps an Anki configuration's discovery while the configuration is unchanged,
  as the extension does, and drops it when it has errors, when preflight fails
  or finds the note invalid, and when an add fails;
- writes Anki fields and the Yomitan API's fields under Compact popup and Anki
  (#673): a row's glosses joined by " | ", Yomitan's compact gloss styles, and
  Smaller Anki cards unwrapping the lists those styles show inline; the switch
  is part of the Anki configuration, so toggling it refuses a stale Add.

Behaviour 0.5.1 adds, which the relay follows:

- accepts the options 0.5.1 writes: `googleDocs` and `youtubeMining` as
  ordinary options, `userScripts` (with a stored `customPopupJavascript`
  migrated to one script), the video mining media options
  (`videoAudioFormat`, `videoAudioBitrate`, `videoImageFormat`,
  `videoImageQuality`), the experimental `crunchyrollMining` switch and any
  activation key name, `MouseRight` included; the new global options are not
  per profile;
- treats a Crunchyroll mining request as Netflix and YouTube ones: its media
  stay with the linked browser, and Add keeps the fields a cue would fill;
- refuses an Anki write that AnkiConnect rejects because an open Browser or
  Edit window shows the note as a definite failure, with media released and an
  explanation (#698), instead of "could not be confirmed";
- renders `{miscinfo}`, the request's `miscInfo` or else its document title,
  and keeps Yomitan API field markers' case (#711);
- appends Settings' pasted known words (`hd_known_words_add`);
- leaves Bee's Ultimate Kanji Dictionary's character ranks out of word lookups'
  frequencies; the clicked-kanji route keeps them (#724);
- answers word status from each profile's first Template across All of Anki:
  with highlighting on and a narrower duplicate scope, the profile's index
  keeps a second, all-decks snapshot, and a confirmed write updates both at one
  row revision. Without it, word status is unavailable (`statuses: null`).

Video media themselves (sentence audio as MP3 or Opus, GIF, WebP or AVIF) are
recorded by the reading browser, which a linked browser never does, so the
relay has none of their checks.

Against `8603ae5` one request differs, and the relay answers as `d959a09` and
later do: the Yomitan API's `ankiCardFormats` with `profileIndex: null`, which
`8603ae5` rejects and `d959a09` reads as profile 0 (`profileIndex ?? 0`).

Language profiles are part of the sharing protocol (Hachidori's
docs/sharing.md, "Languages", and docs/languages.md 17.3-17.6). The relay hosts
them as the extension's `background-sharing.js` does:

- Its hello advertises `language-profiles-v1` once the records have a second
  language, and then gives every browser the records' Japanese view until it
  opts in. `hd_language_profiles_opt_in` is accepted with the records as
  stored, or refused with `missingLanguages` and the Japanese view when they
  hold a language the browser has no pack for. The relay's own packs
  (`hostLanguages`) are Japanese and every pack the extension ships.
- A browser that has not opted in has the `profileId` of its requests removed,
  so they act on Japanese, and its profile requests are refused
  (`PROFILES_NOT_SHARED`). An opted-in browser's `hd_profile_add` is refused
  for a language the relay has no pack for ("The linked Hachidori does not have
  {name}. Update it and try again."). A reply that carries a language a
  browser lacks is sent as its Japanese view, and its link is closed.
- Change frames go out as stored to opted-in browsers and as the Japanese
  view to the rest. Records that gain profiles, or return to Japanese alone,
  close every link once its requests are answered, so each says hello again;
  a link that lacks a language the records gain is refused and closed; a
  refused link whose packs now cover the records is closed so it can opt in.
  A closing link's requests are refused with `CHANGING_LANGUAGES`. The
  extension holds change frames back while it decides; the relay decides under
  the store's lock, so no frame is sent from an older decision.
- Lookups, personal-dictionary saves, the dictionary list's per-profile view,
  the settings worker's writes, counts and marks, and imports (`profileId` of an
  upload) act on the profile a request names.
- The relay's pull session from a browser host opts in with the relay's packs,
  so the relay mirrors the records as stored (see below).

- The Yomitan API answers from the profile its `profileIndex` selects, as
  `background-sharing.js`'s `apiProfile` does: absent, `null` and `0` select
  the Japanese profile, `1`, `2` and so on the other profiles in Settings
  order. An index past the last answers from profile 0, except for
  `ankiCardFormats`, which refuses it with Yomitan's message. Lookups and kanji
  name the profile, the dictionary list and downloads are the profile's view
  (without another language's personal package), Anki fields are written in
  the profile's language with each dictionary's target language, the
  pronunciation fills `{language}` from its pack, and `/tokenize` gives no
  readings in a language without ruby. The HTTP API forwards a numeric
  `profileIndex` on every request that has a body.

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
session open to it, with the API session's capability (`hoshidicts-api-v1`) and
its own origin, `relay://hachidori-relay`. The extension exempts the API's
origin (`relay://yomitan-api`) from the reconnections that language changes
force, because the API keeps no mirror; the pull session keeps one, so it must
reconnect and opt in again like any linked browser, and the browser lists it
among its linked browsers. When the browser's hello offers
`language-profiles-v1`, the session opts in with the relay's packs and mirrors
the opt-in's snapshot in place of the hello's (change frames that arrive before
the reply are dropped, as that snapshot is newer). A refused opt-in mirrors the
Japanese view and is logged.

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
  capabilities: ["hoshidicts-api-v1", "linked-import-v1", "linked-anki-v2"], snapshot}`, plus `language-profiles-v1` while the records have profiles (and the snapshot is then their Japanese view). The snapshot is the six keys,
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
    type's notes. With language profiles, each profile's first Template is a
    source of its own (its language's note-type families: none outside
    Japanese), with its own snapshot and 30-minute schedule, pulled one at a
    time in profile order under one row revision; with one source the record
    keeps the single-snapshot shape. A request that names a profile
    (`profileId`) reads that profile's Templates, whose ids become profile keys
    (`profileTemplateKey`), and its status says `emptyDeck` when only the deck
    is missing. Each change of its rows or source reaches linked browsers as a
    storage frame and a `word-status` frame.
  - A note mined in a profile follows its language (upstream docs/languages.md
    12): glossary text takes its language by script and each dictionary's
    target language (`dictionaryTargetLanguages`), a right-to-left profile or
    dictionary roots the glossary in its direction and marks each block of the
    other one, the sentence and right-to-left dictionary names keep their own
    direction, `{part-of-speech}` reads the pack's labels, and without ruby the
    furigana markers give the plain headword and the sentence is not split.
    The sentence's split (`hd_segment`) and every other engine request the
    relay makes for Anki or the API carry the profile's fields, as the
    extension's relay to its offscreen document adds them. A target language
    without a pack takes the platform's likely script for its language
    (`scripts/language-names.mjs`); unlike `Intl.Locale`, a region does not
    change it.
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
    Yomitan JSON list), downloaded by the relay itself. A URL Template's
    `{language}` is the `host.audioLanguage` of the note's profile's pack
    (upstream docs/languages.md 13), Japanese's `ja` without profiles.
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
shared storage after every step. It repeats a shorter sequence over records an
earlier release stored or that were damaged (upstream's migration cases: the
removed hover delay, the renamed blur-count switch, legacy Anki settings and
links, the old recorder settings, endpoint URLs, malformed word marks), so
both sides migrate them alike. `tests/conformance/anki-oracle.test.mjs` does the
same for Anki: both sides use one fake AnkiConnect (`anki-fake.mjs`, a small
collection with Anki's search syntax and its add check), and each reply, the
stored duplicate index, the queries sent to Anki's browser and the notes sent to
Anki's add check (the rendered fields) must agree. Each submission runs on both
sides from the same collection, and the notes and media files each leaves in it
must agree too; the fake also serves the pronunciations. The relay holds the
extension's fixture dictionary, whose image and styles the extension's side reads
from the relay's engine, and the fake can fail part-way (`state.faults`: a refused
action, a connection lost before or after the action ran, a media store not kept,
the collection edited around an action), so unconfirmed writes, failed readbacks,
add-on rewrites and lost media are compared too, and a second Template, on
another note type, deck and duplicate scope, is chosen by `templateId` in
status, view, setup, preflight, submit and browse requests. Senren and Lapis
notes are mined with the Templates upstream's `applyAnkiPreset` fills, after a
setup proposal for Senren, with the page's selection, title, address and search
and the sentence with furigana (the extension's side splits it with the relay's
engine), and a kanji entry is mined into a note with its character, readings and
stroke count.
`tests/conformance/anki-index.test.mjs` follows the duplicate index through a
relay's restarts, with the stored attempt set around the clock: a recent pull,
even a failed one, is not repeated; an interrupted one is retried at once; a
failed or malformed pull keeps the previous snapshot; a duplicate-scope change
replaces the index at once and a duplicate-policy change keeps it; and after an
endpoint change the old collection's rows are never answered.
`tests/conformance/anki-index-languages.test.mjs` follows upstream's
`anki-index-languages.test.mjs` with two profiles: one snapshot and attempt per
source in profile order, answered for the profile a request names; a repair
in one profile moving the shared row revision; each source's own backoff after
a failed pull; and a profile's new source dropping only its old snapshot, back
to the single record when one source is left. Chromium's quoting of a scoped
style's `[data-dictionary=...]` selector, which jsdom does not apply, is the one
normalisation. The extension's side
renders in jsdom with its own `buildAnkiResourceFields` and downloads with its
own `exportAnkiAudio`, with a stand-in for the browser's audio decoding. The option normalisation and the custom
dictionary's parser and semantic revision are also checked against 4,321 cases
recorded from the extension's code (`scripts/settings-cases.mjs`), and notes in
each language profile against 30 cases its note builder made from upstream's
language scenarios (`scripts/anki-language-cases.mjs`: Japanese, English,
Arabic, Korean and Chinese results, with and without profiles, full and Smaller
Anki cards).

## The engine

The relay links Hoshidicts (`third_party/hoshidicts`, the extension's engine)
natively, built by cmake with the extension's `wasm/bindings.cpp` API. It loads
the enabled dictionaries of the stored `dictionaryState` in order, every kind
each package has (terms, frequencies, pitches, kanji), and reloads when that list
changes. A package that fails to load is skipped, and its error is listed in
`hd_status`. The engine runs on blocking threads, one request at a time.
`hd_segment` looks up every position of a chunk and keeps every result, but
reads only their words, frequencies and dictionaries, so its lookups decompress
no glossary and collect no pitches, as the extension's do from 0.5.0.

`tests/conformance/host-oracle.test.mjs` checks this against the real extension.
It boots the extension's engine service on its own wasm build, imports its test
dictionaries, mirrors them into the relay, and requires the same reply from both
for every request it sends. `tests/conformance/mutations-oracle.test.mjs` does
the same for the mutations: it builds, appends to, re-saves (CRLF, and 2,500 lines across
the builder's 1000-row banks) and empties the custom dictionary, reorders, disables and removes dictionaries, and sends refused and
invalid requests, and requires the same replies, the same stored dictionary state
and custom source after every step, and the same lookups over the result.
`tests/conformance/profiles-oracle.test.mjs` does the same for language profiles,
sending each request as a page of the extension does (its background names the
profile's fields): it adds and removes languages (with an erase, and a stale one),
saves and appends to English's personal dictionary beside Japanese's, edits each
profile's view of the dictionary list, refuses an older client's removal of a
package another language uses, and looks words up by profile.
`tests/conformance/sharing-oracle.test.mjs` links the same browsers to the
extension's own sharing host, through an in-memory relay, and to the relay:
older browsers, profile-aware ones with and without a language, and the
records gaining and losing languages. Every hello, opt-in, reply, change frame
and forced reconnection must agree.
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

- Backups, restoring one, automatic snapshots and resetting lookup counts are
  not ported: they are the extension's own Settings features, which the
  sharing protocol does not forward. A dictionary download is the only archive
  the relay builds.
- `hd_language_selection_write`, which records the language a browser's
  Settings shows, has no relay counterpart: the relay has no Settings page.
- The extension holds change frames back while it decides which view each
  link gets; the relay decides under its store's lock, so no frame goes out
  from an older decision and none needs holding.
- The relay's pull from a browser host is one more linked browser there (it
  opts in to profiles with the relay's packs), and the browser host's Sharing
  page lists it.
- Adding or removing a language profile runs in the relay's process, so no
  reply can be lost between deciding and storing it; the extension reads its
  records back after a lost reply (`profileCasReadback`), and the relay has
  nothing to read back.
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
  `hachidori-relay/<version>`. Its capabilities are the extension's host list:
  `linked-anki-v1`, `linked-anki-v2`, `hoshidicts-api-v1` and
  `linked-import-v1`, and `language-profiles-v1` while the records have profiles. A reader that predates Templates mines with the first
  one and edits its legacy fields, as with the extension.
- The recommended-dictionary installer's entries report their download bytes,
  the installing phase and the installation's duration, as the extension's do.
  Linked pages learn of them by asking, every four seconds, as they already
  do; there are no `hd_setup_progress` events. Startup's onboarding record
  (`setupState`) is never written, as the extension does not write it for a
  shared request.
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
- The duplicate index reads only the note fields it keys (`notesInfo`'s
  `noteFields`, which the AnkiConnect fork at
  [JSchoreels/anki-connect](https://github.com/JSchoreels/anki-connect)
  `3f3cb0a` accepts); the extension reads whole notes. 0.5.0's Anki caching
  keeps configurations and mines in one request but still reads whole notes on
  every pull and live lookup, so the projection still pays on a large
  collection. An AnkiConnect that rejects the parameter is asked once; the
  relay then reads whole notes until it restarts.
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
