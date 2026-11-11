# Language fixtures

Copied from Hachidori at bf1a78f8298b621ce582accbff12f6caa0970806 (0.5.0, GPL-3.0)
for src/host/engine/languages.rs, the port of its `test/node-smoke-languages.mjs`:

- `hachidori-fixture.zip`: the Japanese fixture, `buildFixtureZip()` of `test/make-fixture.mjs`.
- `hachidori-english-fixture.zip`, `hachidori-arabic-fixture.zip`: `buildLanguageFixtureZip`
  of `test/make-language-fixtures.mjs` (deterministic).
- `packs/en`, `packs/ar`: the fixture packs of `test/data/languages/`, without their tests.
- `languages-japanese-replies.json`: `test/data/languages-japanese-replies.json`, the SHA-256
  of the engine's replies to Japanese requests without language keys, recorded before the
  language ABI.
