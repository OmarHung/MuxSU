import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  candidatePairs,
  decodeText,
  DEFAULT_MIN_CONFIDENCE,
  importScan,
  promote,
  routeAnswer,
  sameIdentity,
  settleLocally,
  stateFor,
} from "./identity-oracle.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const readJson = (path) => JSON.parse(readFileSync(join(root, path), "utf8"));

const corpus = readJson("docs/identity-oracle/corpus.json");
const identity = (id) => {
  const found = corpus.identities.find((entry) => entry.id === id);
  assert.ok(found, `${id} is missing from the corpus`);
  return found;
};

/** A Score/Noul answer set shaped like the SDK returns one. */
function answers(score, confidence, nouls = {}) {
  return {
    relation: { type: "score", score, confidence },
    sameProductLine: { type: "noul", noul: nouls.sameProductLine ?? 0.5 },
    modeChangeExplainsDifference: { type: "noul", noul: nouls.modeChangeExplainsDifference ?? 0.5 },
    differentPanelSize: { type: "noul", noul: nouls.differentPanelSize ?? 0.5 },
  };
}

test("an absent serial number is unknown rather than a difference", () => {
  const withSerial = { manufacturerId: "MSI", productCode: "3CF0", serialNumber: "PC-SERIAL" };
  const withoutSerial = { manufacturerId: "msi", productCode: "3cf0", serialNumber: null };

  assert.equal(sameIdentity(withSerial, withoutSerial), true);
  assert.equal(sameIdentity(withSerial, { ...withSerial, serialNumber: "OTHER" }), false);
  assert.equal(sameIdentity(withoutSerial, { ...withoutSerial, productCode: "7CF0" }), false);
});

test("two serials from the same EDID field that differ are settled in code, not by the model", () => {
  const settled = settleLocally(identity("acr-0725-th6tt0028525"), identity("msi-3cf0-cf0h246200009"));

  assert.equal(settled?.outcome, "different");
  assert.match(settled.reason, /same EDID field/);
});

/** One ASUS VG252Q shared between these two computers reads as `TH6TT0028525`
 *  on Windows and `576726074` on macOS, because Windows reports the EDID's
 *  serial-text descriptor and macOS its 32-bit numeric serial. Comparing those
 *  concluded one display was two. */
test("serials from different EDID fields are not compared at all", () => {
  const macOs = identity("asus-vg252q");
  const windows = identity("acr-0725-th6tt0028525");

  assert.notEqual(macOs.serialNumber, windows.serialNumber);
  assert.notEqual(macOs.serialField, windows.serialField);
  assert.equal(settleLocally(macOs, windows), null, "this pair has to reach the model");

  const state = stateFor(macOs, windows);
  assert.match(state.howTheseWereRead, /different EDID fields/);
  assert.match(state.howTheseWereRead, /says nothing about whether these are one display or two/);
});

test("the same field on both sides is said to be comparable", () => {
  const state = stateFor(identity("msi-3cf0-cf0h246200009"), identity("msi-7cf0-cf0h246200009"));

  assert.match(state.howTheseWereRead, /same EDID field, so they can be compared directly/);
  assert.equal(state.displayA.serialNumberRead, state.displayB.serialNumberRead);
  assert.match(state.displayA.whichSerialFieldThatCameFrom, /WMI SerialNumberID/);
});

test("an identity with no serial says nothing about serial fields", () => {
  const state = stateFor(identity("msi-mpg274u-uhd"), identity("msi-mpg274u-fhd"));

  assert.equal(state.displayA.whichSerialFieldThatCameFrom, null);
  assert.ok(!state.howTheseWereRead.includes("EDID field"));
});

test("two identities one scan listed together are settled in code as two panels", () => {
  const settled = settleLocally(identity("msi-mpg274u-uhd"), identity("asus-vg252q"));

  assert.equal(settled?.outcome, "different");
  assert.match(settled.reason, /one scan/);
});

test("state says what is on record about simultaneity and never asserts more", () => {
  const liveScanPair = stateFor(identity("msi-mpg274u-uhd"), identity("asus-vg252q"));
  const fixturePair = stateFor(identity("aoc-24b2hm2"), identity("aoc-24b2w1"));

  assert.match(liveScanPair.howTheseWereRead, /no scan has ever listed the two of them together/);
  assert.match(fixturePair.howTheseWereRead, /no record either way/);
  assert.ok(!fixturePair.howTheseWereRead.includes("never seen side by side"));
});

test("the two display modes of one panel are left for the model to judge", () => {
  const { settled, open } = candidatePairs(corpus.identities);
  const isMsiModePair = (pair) =>
    [pair.left.id, pair.right.id].sort().join("|") === "msi-mpg274u-fhd|msi-mpg274u-uhd";

  const count = corpus.identities.length;
  assert.ok(open.some(isMsiModePair), "MSI:3CF0 vs MSI:7CF0 must reach the model");
  assert.ok(!settled.some(isMsiModePair));
  assert.equal(settled.length + open.length, (count * (count - 1)) / 2, "every unordered pair is accounted for");
  assert.ok(settled.length > 0 && open.length > 0, "both paths are exercised by the real corpus");
});

test("state carries the evidence a judgment needs and no internal ids", () => {
  const state = stateFor(identity("msi-mpg274u-uhd"), identity("msi-mpg274u-fhd"));

  assert.equal(state.displayA.edidProductCode, "3CF0");
  assert.equal(state.displayB.resolutionWhenObserved, "1920x1080");
  assert.equal(state.displayA.serialNumberRead, "none read from this display");
  assert.ok(!JSON.stringify(state).includes("msi-mpg274u-uhd"));
});

test("a score rounds to the nearest level", () => {
  assert.equal(routeAnswer(answers(0.4, 0.9)).outcome, "different");
  assert.equal(routeAnswer(answers(1.2, 0.9)).outcome, "ask");
  assert.equal(routeAnswer(answers(1.8, 0.9)).outcome, "same");
});

test("a low-confidence 'same' becomes a question for the curator", () => {
  const routed = routeAnswer(answers(2, DEFAULT_MIN_CONFIDENCE - 0.01));

  assert.equal(routed.outcome, "ask");
  assert.equal(routed.downgradedByConfidence, true);
  assert.equal(routed.level, 2, "the raw judgment stays visible");
});

test("the nouls ride along as reasons", () => {
  const routed = routeAnswer(answers(2, 0.9, { differentPanelSize: 0.02, sameProductLine: 0.97 }));

  assert.equal(routed.reasons.differentPanelSize, 0.02);
  assert.equal(routed.reasons.sameProductLine, 0.97);
});

/** A fixed two-entry corpus, so importing is tested against something that does
 *  not move as the real corpus grows. These are what macOS read of the two
 *  displays it shares with the Windows machine. */
const seedCorpus = {
  identities: [
    {
      id: "msi-mpg274u-uhd",
      manufacturerId: "MSI",
      productCode: "3CF0",
      productName: "MPG 274U E16M",
      serialNumber: null,
      serialField: null,
      observedResolution: "3840x2160",
      observedOn: "macOS",
      observedAlongside: ["asus-vg252q"],
    },
    {
      id: "asus-vg252q",
      manufacturerId: "ACR",
      productCode: "0725",
      productName: "VG252Q",
      serialNumber: "576726074",
      serialField: "edidNumericSerial",
      observedResolution: "1920x1080",
      observedOn: "macOS",
      observedAlongside: ["msi-mpg274u-uhd"],
    },
  ],
};

/** One `muxsu-cli list --json` scan of the same two displays from Windows, with
 *  the MSI set to 1080p — MonitorDescriptor in camelCase, its fingerprint in
 *  snake_case, and Windows reading serials from the EDID field macOS does not. */
const windowsScan = [
  {
    id: "windows:\\\\?\\DISPLAY#MSI7CF0#5&1234#0",
    name: "MPG 274U E16M",
    fingerprint: { manufacturer_id: "MSI", product_code: "7CF0", serial_number: "CF0H246200009" },
    active: true,
    builtIn: false,
    maxResolution: { width: 1920, height: 1080 },
  },
  {
    id: "windows:\\\\?\\DISPLAY#ACR0725#5&5678#0",
    name: "VG252Q",
    fingerprint: { manufacturer_id: "ACR", product_code: "0725", serial_number: "TH6TT0028525" },
    active: true,
    builtIn: false,
    maxResolution: { width: 1920, height: 1080 },
  },
  {
    id: "windows:\\\\?\\DISPLAY#BOE0A1B#5&9999#0",
    name: "Internal panel",
    fingerprint: { manufacturer_id: "BOE", product_code: "0A1B", serial_number: null },
    active: true,
    builtIn: true,
    maxResolution: { width: 2560, height: 1600 },
  },
];

test("a scan adds what a host read and skips its built-in panel", () => {
  const imported = importScan(seedCorpus, windowsScan, "Windows 11 desktop, MSI set to 1920x1080");

  assert.equal(imported.platform, "Windows");
  assert.equal(imported.scanned, 2, "the built-in panel is not corpus material");
  const added = imported.corpus.identities.find((entry) => entry.serialNumber === "CF0H246200009");
  assert.equal(added.productCode, "7CF0");
  assert.equal(added.observedResolution, "1920x1080");
  assert.equal(added.observedOn, "Windows");
  assert.equal(added.serialField, "edidSerialText");
  assert.equal(added.provenance, "Windows 11 desktop, MSI set to 1920x1080");
  assert.ok(!imported.corpus.identities.some((entry) => entry.manufacturerId === "BOE"));
});

/** The two hosts read different EDID fields, so one shared panel arrives under
 *  two serials. Both observations are kept: collapsing them would throw away
 *  which host read what, and that is what says whether two serials can be
 *  compared at all. */
test("a host reading a different serial field gets its own entry", () => {
  const imported = importScan(seedCorpus, windowsScan, "note");
  const asus = imported.corpus.identities.filter((entry) => entry.manufacturerId === "ACR");

  assert.equal(imported.added, 2, "both of the Windows readings are new");
  assert.deepEqual(
    asus.map((entry) => `${entry.serialNumber}/${entry.serialField}`),
    ["576726074/edidNumericSerial", "TH6TT0028525/edidSerialText"],
  );
  assert.equal(settleLocally(asus[0], asus[1]), null, "one panel must not be settled as two");
});

test("everything one scan listed is recorded as seen together", () => {
  const imported = importScan(seedCorpus, windowsScan, "note");
  const msi = imported.corpus.identities.find((entry) => entry.serialNumber === "CF0H246200009");
  const asus = imported.corpus.identities.find((entry) => entry.serialNumber === "TH6TT0028525");
  const macOsAsus = imported.corpus.identities.find((entry) => entry.id === "asus-vg252q");

  assert.ok(msi.observedAlongside.includes(asus.id));
  assert.ok(asus.observedAlongside.includes(msi.id));
  assert.equal(settleLocally(msi, asus)?.outcome, "different");
  assert.deepEqual(macOsAsus.observedAlongside, ["msi-mpg274u-uhd"], "an earlier scan is left alone");
});

test("re-importing the same scan changes nothing", () => {
  const once = importScan(seedCorpus, windowsScan, "note");
  const twice = importScan(once.corpus, windowsScan, "note");

  assert.equal(twice.added, 0);
  assert.deepEqual(twice.corpus.identities, once.corpus.identities);
});

test("an import without provenance is refused", () => {
  assert.throws(() => importScan(seedCorpus, windowsScan, undefined), /--note/);
  assert.throws(() => importScan(seedCorpus, { not: "an array" }, "note"), /muxsu-cli list --json/);
});

test("two product codes carrying one serial reach the model with that evidence", () => {
  const imported = importScan(seedCorpus, windowsScan, "note");
  // A second scan of the same panel, this time in 4K mode.
  const uhdOnWindows = {
    id: "windows:\\\\?\\DISPLAY#MSI3CF0#5&1234#0",
    name: "MPG 274U E16M",
    fingerprint: { manufacturer_id: "MSI", product_code: "3CF0", serial_number: "CF0H246200009" },
    builtIn: false,
    maxResolution: { width: 3840, height: 2160 },
  };
  const both = importScan(imported.corpus, [uhdOnWindows], "Windows 11 desktop, MSI set to 3840x2160");
  const find = (code) =>
    both.corpus.identities.find((entry) => entry.productCode === code && entry.serialNumber === "CF0H246200009");

  // An identical serial is not a reason for code to declare a merge — only the
  // user may — so the pair still reaches the model, now carrying the evidence.
  assert.equal(settleLocally(find("3CF0"), find("7CF0")), null);
  const state = stateFor(find("3CF0"), find("7CF0"));
  assert.equal(state.displayA.serialNumberRead, "CF0H246200009");
  assert.equal(state.displayB.serialNumberRead, "CF0H246200009");
  assert.match(state.howTheseWereRead, /same EDID field/);
});

/** `cargo run -p muxsu-cli -- list --json > scan.json` in Windows PowerShell
 *  5.1 writes UTF-16LE with a byte-order mark, which read as UTF-8 fails on the
 *  very first character. Notepad's UTF-8 mark does the same to a proposals file
 *  edited by hand. */
test("a scan is read whatever encoded it", () => {
  const json = JSON.stringify(windowsScan);
  const utf16le = Buffer.concat([Buffer.from([0xff, 0xfe]), Buffer.from(json, "utf16le")]);
  const utf16be = Buffer.concat([Buffer.from([0xfe, 0xff]), Buffer.from(json, "utf16le").swap16()]);
  const utf8Bom = Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from(json, "utf8")]);

  for (const [label, bytes] of [["UTF-16LE", utf16le], ["UTF-16BE", utf16be], ["UTF-8 with a mark", utf8Bom], ["plain UTF-8", Buffer.from(json, "utf8")]]) {
    assert.deepEqual(JSON.parse(decodeText(bytes)), windowsScan, `${label} must decode`);
  }
});

test("non-ASCII survives every encoding a person might hand us", () => {
  const value = { note: "MSI 螢幕 3840×2160", em: "—" };
  const json = JSON.stringify(value);

  assert.deepEqual(
    JSON.parse(decodeText(Buffer.concat([Buffer.from([0xff, 0xfe]), Buffer.from(json, "utf16le")]))),
    value,
  );
  assert.deepEqual(
    JSON.parse(decodeText(Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from(json, "utf8")]))),
    value,
  );
});

test("only a human-approved proposal is promoted", () => {
  const table = { version: 1, groups: [] };
  const proposal = {
    left: { manufacturerId: "ACR", productCode: "0725", productName: "VG252Q" },
    right: { manufacturerId: "ACR", productCode: "0726", productName: "VG252Q" },
    outcome: "same",
    score: 2,
    confidence: 0.9,
  };

  assert.equal(promote(table, { proposals: [{ ...proposal, approved: null }] }).added, 0);
  assert.equal(promote(table, { proposals: [{ ...proposal, approved: false }] }).added, 0);
  assert.equal(promote(table, { proposals: [{ ...proposal, outcome: "ask", approved: true }] }).added, 0);

  const promoted = promote(table, { proposals: [{ ...proposal, approved: true }] });
  assert.equal(promoted.added, 2);
  assert.deepEqual(promoted.table.groups[0].identities, [
    { manufacturerId: "ACR", productCode: "0725" },
    { manufacturerId: "ACR", productCode: "0726" },
  ]);
  assert.equal(promoted.table.groups[0].source, "curator");
  assert.deepEqual(table.groups, [], "the table passed in is not mutated");
});

test("a third identity joins the panel's existing group instead of starting a new one", () => {
  const table = readJson("src-tauri/data/known-identity-groups.json");
  const promoted = promote(table, {
    proposals: [
      {
        left: { manufacturerId: "MSI", productCode: "7CF0" },
        right: { manufacturerId: "MSI", productCode: "5CF0" },
        outcome: "same",
        score: 2,
        confidence: 0.9,
        approved: true,
      },
    ],
  });

  assert.equal(promoted.added, 1);
  assert.equal(promoted.table.groups.length, table.groups.length);
  assert.deepEqual(
    promoted.table.groups[0].identities.map((entry) => entry.productCode),
    ["3CF0", "7CF0", "5CF0"],
  );
});

test("promoting the same pair twice adds nothing the second time", () => {
  const table = readJson("src-tauri/data/known-identity-groups.json");
  const proposals = {
    proposals: [
      {
        left: { manufacturerId: "MSI", productCode: "3CF0" },
        right: { manufacturerId: "MSI", productCode: "7CF0" },
        outcome: "same",
        score: 2,
        confidence: 0.9,
        approved: true,
      },
    ],
  };

  assert.equal(promote(table, proposals).added, 0);
});

test("the shipped table records the equivalence product-facts verified on hardware", () => {
  const table = readJson("src-tauri/data/known-identity-groups.json");
  const msi = table.groups.find((group) => group.identities.some((entry) => entry.productCode === "3CF0"));

  assert.equal(msi.source, "product-facts", "a hardware-verified group is not attributed to the model");
  assert.deepEqual(
    msi.identities.map((entry) => `${entry.manufacturerId}:${entry.productCode}`).sort(),
    ["MSI:3CF0", "MSI:7CF0"],
  );
  assert.ok(readFileSync(join(root, "product-facts.md"), "utf8").includes("7CF0"));
});
