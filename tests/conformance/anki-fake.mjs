// SPDX-License-Identifier: GPL-3.0-or-later
// A fake AnkiConnect over HTTP for the Anki oracle: one small collection, the
// search syntax the extension's queries use, and a log of what each request did.
// It answers through Hachidori's own `answerAnkiConnect`, so envelopes and `multi`
// behave as its tests expect of the add-on.

import http from "node:http";

const KIKU = ["Expression", "ExpressionFurigana", "ExpressionReading", "ExpressionAudio", "RelatedExpression", "SelectionText",
  "MainDefinition", "DefinitionPicture", "Sentence", "SentenceFurigana", "SentenceTranslation", "SentenceAudio", "Picture", "Glossary",
  "Hint", "IsWordAndSentenceCard", "IsClickCard", "IsSentenceCard", "IsAudioCard", "PitchPosition", "PitchCategories", "Frequency",
  "FreqSort", "MiscInfo"];
const LAPIS = KIKU.filter((field) => !["RelatedExpression", "SentenceTranslation"].includes(field));

export function collection() {
  const models = {
    Kiku: { id: 1001, fields: KIKU },
    "Lapis v1": { id: 1002, fields: LAPIS },
    Basic: { id: 1003, fields: ["Front", "Back"] },
  };
  const decks = ["Default", "Mining", "Mining::Sub"];
  const note = (noteId, modelName, deckName, first, mature = false) => ({
    noteId, modelName, deckName, mature,
    fields: Object.fromEntries(models[modelName].fields.map((field, index) => [field, index === 0 ? first : ""])),
  });
  const notes = [
    note(1, "Kiku", "Mining", "猫", true),
    note(2, "Kiku", "Mining", "犬"),
    note(3, "Lapis v1", "Mining::Sub", "猫"),
    note(4, "Basic", "Default", "猫"),
    note(5, "Kiku", "Default", "ABC"),
    note(6, "Kiku", "Mining", "a&lt;b"),
    note(7, "Kiku", "Mining", "鳥", true),
  ];
  return { models, decks, notes };
}

// Anki's search, as far as these queries go: quoted or bare terms, `-` negation,
// adjacency as "and", `or`, and parentheses.
function tokens(query) {
  const out = [];
  let at = 0;
  while (at < query.length) {
    const c = query[at];
    if (c === " ") at += 1;
    else if (c === "(" || c === ")") {
      out.push(c);
      at += 1;
    } else {
      let negated = false;
      if (c === "-") {
        negated = true;
        at += 1;
      }
      let text = "";
      if (query[at] === "\"") {
        at += 1;
        while (at < query.length && query[at] !== "\"") {
          if (query[at] === "\\") {
            text += query.slice(at, at + 2);
            at += 2;
          } else text += query[at++];
        }
        at += 1;
      } else {
        while (at < query.length && !" ()".includes(query[at])) text += query[at++];
      }
      out.push(!negated && text.toLowerCase() === "or" ? "or" : { text, negated });
    }
  }
  return out;
}

const unescape = (text) => text.replace(/\\(.)/gu, "$1");
const fold = (text) => text.normalize("NFC").replace(/[A-Z]/gu, (c) => c.toLowerCase());

function matches(state, note, { text, negated }) {
  const colon = text.search(/(?<!\\):/u);
  const key = colon < 0 ? "" : unescape(text.slice(0, colon)).toLowerCase();
  const value = colon < 0 ? unescape(text) : unescape(text.slice(colon + 1));
  const model = state.models[note.modelName];
  let result;
  if (key === "note") result = note.modelName.toLowerCase() === value.toLowerCase();
  else if (key === "deck") {
    const deck = note.deckName.toLowerCase();
    result = deck === value.toLowerCase() || deck.startsWith(`${value.toLowerCase()}::`);
  } else if (key === "mid") result = model.id === Number(value);
  else if (key === "nid") result = value.split(",").map(Number).includes(note.noteId);
  else if (key === "is") result = value === "review" ? note.mature : false;
  else if (key === "prop") result = value === "ivl>=21" ? note.mature : false;
  else if (key === "dupe") {
    const [mid, first] = [value.slice(0, value.indexOf(",")), value.slice(value.indexOf(",") + 1)];
    result = model.id === Number(mid) && note.fields[model.fields[0]] === first;
  } else if (key) {
    const field = model.fields.find((name) => name.toLowerCase() === key);
    result = field !== undefined && fold(note.fields[field]) === fold(value);
  } else result = Object.values(note.fields).some((field) => fold(field).includes(fold(value)));
  return negated ? !result : result;
}

function search(state, query) {
  const list = tokens(query);
  let at = 0;
  const expression = () => {
    const clauses = [conjunction()];
    while (list[at] === "or") {
      at += 1;
      clauses.push(conjunction());
    }
    return (note) => clauses.some((clause) => clause(note));
  };
  const conjunction = () => {
    const parts = [];
    while (at < list.length && list[at] !== "or" && list[at] !== ")") {
      if (list[at] === "(") {
        at += 1;
        parts.push(expression());
        at += 1;
      } else {
        const term = list[at++];
        parts.push((note) => matches(state, note, term));
      }
    }
    return (note) => parts.every((part) => part(note));
  };
  const test = expression();
  return state.notes.filter((note) => test(note));
}

const cardId = (note) => note.noteId * 10;

// Anki's add check: an empty first field, then its duplicate rule (the first field
// within the note type, across the collection or within the named deck).
function addCheck(state, note) {
  const model = state.models[note.modelName];
  if (!model) return { canAdd: false, error: `model was not found: ${note.modelName}` };
  if (!state.decks.includes(note.deckName)) return { canAdd: false, error: `deck was not found: ${note.deckName}` };
  const first = note.fields?.[model.fields[0]] ?? "";
  if (!first.trim()) return { canAdd: false, error: "cannot create note because it is empty" };
  const options = note.options ?? {};
  if (options.allowDuplicate === true) return { canAdd: true };
  const deck = options.duplicateScope === "deck" ? (options.duplicateScopeOptions?.deckName ?? note.deckName) : null;
  const duplicate = state.notes.some((other) => other.modelName === note.modelName && fold(other.fields[model.fields[0]]) === fold(first)
    && (deck === null || other.deckName.toLowerCase() === deck.toLowerCase()));
  return duplicate ? { canAdd: false, error: "cannot create note because it is a duplicate" } : { canAdd: true };
}

// A note written through AnkiConnect, after Anki's own add check.
function addNote(state, note, AnkiConnectError) {
  // A note another client adds meanwhile: the add check passed, the add does not.
  if (state.racing.includes(note.fields.Expression)) throw new AnkiConnectError("cannot create note because it is a duplicate");
  const check = addCheck(state, note);
  if (!check.canAdd) throw new AnkiConnectError(check.error);
  const noteId = Math.max(0, ...state.notes.map((entry) => entry.noteId)) + 1;
  const model = state.models[note.modelName];
  state.notes.push({ noteId, modelName: note.modelName, deckName: note.deckName, mature: false, tags: [...note.tags],
    fields: Object.fromEntries(model.fields.map((field) => [field, note.fields[field] ?? ""])) });
  return noteId;
}

// Fault switches: `state.faults` lists faults armed for the next matching
// actions, each used once, in order. A fault names its `action` and, with
// `after`, fires only once that action has run since the faults were armed,
// and with `filter`, only for the parameters it accepts:
// - `error`: the add-on refuses the action with `message`;
// - `edit`: `edit(state, params)` changes the collection before the action
//   runs (`when: "before"`, the default) or after it (`when: "after"`), as an
//   add-on rewrite, an edit in Anki or another client would;
// - `lost-store`: a media store is acknowledged and not kept;
// - `result`: the action answers `result` instead of running (a malformed reply);
// - `drop-before` and `drop-after`: the connection closes before the request
//   is dispatched, or after it ran, so its outcome is unknown to the client.
function takeFault(state, action, kinds, params) {
  const index = state.faults.findIndex((fault) => fault.action === action && kinds.includes(fault.kind)
    && (fault.after === undefined || state.ran.includes(fault.after)) && (fault.filter?.(params) ?? true));
  return index < 0 ? null : state.faults.splice(index, 1)[0];
}

// The handler `answerAnkiConnect` runs: each action against the collection.
export function handler(state, AnkiConnectError) {
  const byCard = (cards) => cards.map((id) => state.notes.find((note) => cardId(note) === id)).filter(Boolean);
  return (action, params) => {
    state.log.push({ action, params });
    const fault = takeFault(state, action, ["error", "edit", "lost-store", "result"], params);
    if (fault?.kind === "error") throw new AnkiConnectError(fault.message);
    if (fault?.kind === "result") {
      state.ran.push(action);
      return fault.result;
    }
    if (fault?.kind === "edit" && fault.when !== "after") fault.edit(state, params);
    if (fault?.kind === "lost-store") {
      state.ran.push(action);
      return params.filename;
    }
    const result = run(action, params);
    state.ran.push(action);
    if (fault?.kind === "edit" && fault.when === "after") fault.edit(state, params);
    return result;
  };
  function run(action, params) {
    switch (action) {
      case "version": return 6;
      case "deckNames": return [...state.decks];
      case "modelNames": return Object.keys(state.models);
      case "modelNamesAndIds": return Object.fromEntries(Object.entries(state.models).map(([name, model]) => [name, model.id]));
      case "modelFieldNames": {
        const model = state.models[params.modelName];
        if (!model) throw new AnkiConnectError(`model was not found: ${params.modelName}`);
        return [...model.fields];
      }
      case "findNotes": return search(state, params.query).map((note) => note.noteId);
      case "findCards": return search(state, params.query).map(cardId);
      case "getDecks": {
        const decks = {};
        for (const note of byCard(params.cards)) (decks[note.deckName] ??= []).push(cardId(note));
        return decks;
      }
      case "cardsToNotes": return [...new Set(byCard(params.cards).map((note) => note.noteId))];
      case "notesInfo": return params.notes.map((id) => state.notes.find((note) => note.noteId === id)).filter(Boolean).map((note) => ({
        noteId: note.noteId, modelName: note.modelName, tags: [], cards: [cardId(note)],
        fields: Object.fromEntries(Object.entries(note.fields).map(([name, value], order) => [name, { value, order }])),
      }));
      case "canAddNotesWithErrorDetail": return params.notes.map((note) => addCheck(state, note));
      case "canAddNotes": return params.notes.map((note) => addCheck(state, note).canAdd);
      case "cardsInfo": return byCard(params.cards).map((note) => ({ cardId: cardId(note), note: note.noteId, deckName: note.deckName }));
      case "guiBrowse": return search(state, params.query).map(cardId);
      case "addNote": return addNote(state, params.note, AnkiConnectError);
      case "updateNoteFields": {
        const note = state.notes.find((entry) => entry.noteId === params.note.id);
        if (!note) throw new AnkiConnectError("Note was not found: " + params.note.id);
        for (const [field, value] of Object.entries(params.note.fields)) {
          if (!Object.hasOwn(note.fields, field)) throw new AnkiConnectError("field not found: " + field);
          note.fields[field] = value;
        }
        return null;
      }
      case "storeMediaFile": state.media[params.filename] = params.data; return params.filename;
      case "getMediaFilesNames": return Object.keys(state.media).filter((name) => name === params.pattern);
      case "deleteMediaFile": delete state.media[params.filename]; return null;
      default: throw new AnkiConnectError(`unsupported action: ${action}`);
    }
  }
}

// The actions a request body runs: its own, or a `multi`'s.
const bodyActions = (body) => (body?.action === "multi" ? body.params?.actions ?? [] : [body]);

// The collection over HTTP; `state.log` holds every action, in order.
export async function startAnkiConnect(answerAnkiConnect, AnkiConnectError) {
  const state = { ...collection(), media: {}, log: [], refuse: false, audio: [], racing: [], faults: [], ran: [] };
  const handle = handler(state, AnkiConnectError);
  const server = http.createServer((request, response) => {
    // Pronunciations: /audio/<term>.mp3 plays, /list/<term> lists it, /missing/ is 404.
    if (request.method === "GET") {
      const path = decodeURIComponent(new URL(request.url, "http://fake").pathname);
      state.audio.push(path);
      if (path.startsWith("/audio/")) {
        response.writeHead(200, { "Content-Type": "audio/mpeg" }).end(Buffer.concat([Buffer.from("ID3"), Buffer.from(path)]));
      } else if (path.startsWith("/list/")) {
        const url = `http://${request.headers.host}/audio/${encodeURIComponent(path.slice(6))}.mp3`;
        response.writeHead(200, { "Content-Type": "application/json" })
          .end(JSON.stringify({ type: "audioSourceList", audioSources: [{ url: `http://${request.headers.host}/missing/x`, name: "Gone" }, { url, name: "Speaker" }] }));
      } else response.writeHead(404).end("missing");
      return;
    }
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", async () => {
      if (state.refuse) {
        response.writeHead(403).end();
        return;
      }
      try {
        const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
        const drop = (kind) => bodyActions(body).some((entry) => takeFault(state, entry?.action, [kind], entry?.params));
        if (drop("drop-before")) {
          request.socket.destroy();
          return;
        }
        const dropAfter = drop("drop-after");
        const reply = await answerAnkiConnect(body, handle);
        if (dropAfter) {
          request.socket.destroy();
          return;
        }
        response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify(reply));
      } catch (error) {
        response.writeHead(500).end(String(error));
      }
    });
  });
  await new Promise((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
  return {
    state,
    url: `http://127.0.0.1:${server.address().port}`,
    close: () => new Promise((resolveClose) => server.close(resolveClose)),
  };
}
