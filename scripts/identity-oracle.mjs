/**
 * Dev-time oracle for "these two EDID identities are one physical display".
 *
 * MuxSU cannot derive that equivalence — `product-facts.md` is explicit that
 * only the user may declare it, because a display like the MSI MPG 274U
 * publishes `MSI:3CF0` at 3840x2160 and `MSI:7CF0` at 1920x1080 and every host
 * reads the second as a different display. What the app *can* do is stop making
 * the user guess which of the missing shared displays a newly appeared identity
 * belongs to: `renderMonitorMerge` currently hands them a bare dropdown, and
 * merging the wrong pair is not something they can undo by looking at a screen.
 *
 * This tool runs on a developer machine, never in the app. It asks TypeSafe's
 * Jev to judge candidate pairs from a corpus of observed identities, writes the
 * judgments out for a human to curate, and the approved ones are promoted into
 * a small table that ships with the app. The app then reads that table offline:
 * no API key in the binary, no network on the display-scan path, and the user
 * still makes every declaration.
 *
 *   node scripts/identity-oracle.mjs import --note "..." # a scan -> the corpus
 *   node scripts/identity-oracle.mjs propose             # asks Jev, writes proposals
 *   node scripts/identity-oracle.mjs promote             # approved proposals -> shipped table
 *
 * `propose` needs `TYPESAFE_API_KEY`. Answers are cached by request content, so
 * re-running costs nothing until the corpus or the questions change.
 *
 * Sampling a display that publishes two identities, on the host that reads its
 * serial number — Windows reads the MSI MPG 274U's, macOS reads none — because a
 * serial shared by both product codes is what makes the pair judgeable at all:
 *
 *   cargo run -p muxsu-cli -- list --json > scan-uhd.json
 *   node scripts/identity-oracle.mjs import --scan scan-uhd.json \
 *     --note "Windows 11, MSI MPG 274U at 3840x2160"
 *   # set the display to its other mode in Windows display settings, then:
 *   cargo run -p muxsu-cli -- list --json > scan-fhd.json
 *   node scripts/identity-oracle.mjs import --scan scan-fhd.json \
 *     --note "Windows 11, MSI MPG 274U at 1920x1080"
 *
 * Take one scan per mode, never both at once: `import` records everything a
 * single scan listed as having been seen together, which is how it knows those
 * are separate panels.
 *
 * What the first live run against `jev-1.13.0` showed, on 2026-09-20, over the
 * five identities in the corpus — read this before expecting more of the tool:
 *
 * - Every pair that is two different displays came back as one, at confidence
 *   0.91 to 0.97. That is what the tool is good for: clearing the obvious so a
 *   person only reads the pairs that are genuinely open.
 * - The one pair that really is two modes of one panel (the MSI MPG 274U's
 *   `3CF0` and `7CF0`) came back in the middle level, "ask the person who owns
 *   them", at score 1.21. It is not wrong to. Neither identity carries a serial
 *   number, so the evidence cannot separate one panel in two modes from two
 *   units of that model left in different modes — which is exactly why
 *   `product-facts.md` says only the user may declare it.
 * - So the model has never proposed "same" here, and the confidence gate below
 *   has never fired. Expect this tool to shrink the review pile, not to grow the
 *   shipped table on its own. Entries still arrive by a person's judgment.
 */

import { createHash } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

export const DEFAULT_CORPUS = "docs/identity-oracle/corpus.json";
export const DEFAULT_PROPOSALS = "docs/identity-oracle/proposals.json";
export const DEFAULT_CACHE = "docs/identity-oracle/cache.json";
export const DEFAULT_TABLE = "src-tauri/data/known-identity-groups.json";
export const DEFAULT_MODEL = "jev-1.13.0";

/**
 * Least Score confidence accepted before a pair is proposed as one display.
 * Not tuned: it has never been evaluated against labelled MuxSU data, so it is
 * a starting point to measure, not a validated threshold. A pair below it is
 * still reported — as a question for the curator rather than a proposal.
 */
export const DEFAULT_MIN_CONFIDENCE = 0.6;

/** The three outcomes, ordered as the Score's levels. */
export const OUTCOMES = ["different", "ask", "same"];

// ---------------------------------------------------------------------------
// What code settles on its own
// ---------------------------------------------------------------------------

const normalize = (value) => (value ?? "").trim().toUpperCase();

/** The same rule as `monitor_identity::same_identity` in the app: same model,
 *  and a serial number only counts against a match when both sides have one. */
export function sameIdentity(left, right) {
  if (normalize(left.manufacturerId) !== normalize(right.manufacturerId)) return false;
  if (normalize(left.productCode) !== normalize(right.productCode)) return false;
  const [a, b] = [left.serialNumber, right.serialNumber];
  return a == null || b == null || a === b;
}

/** Whether the corpus records these two identities turning up in one scan. One
 *  panel is enumerated once, whichever mode it is in, so a scan that lists both
 *  is looking at two panels. */
export function seenTogether(left, right) {
  return Boolean(
    left.observedAlongside?.includes(right.id) || right.observedAlongside?.includes(left.id),
  );
}

/**
 * Whether ordinary code already knows how this pair relates, so it never
 * reaches the model. Both rules are deductions from what was observed, not
 * judgment calls: two serials that both exist and differ are two units — that
 * is MuxSU's own documented rule — and two identities enumerated in one scan
 * are two panels.
 */
export function settleLocally(left, right) {
  if (sameIdentity(left, right)) {
    return { outcome: "same", reason: "Already one identity under the app's own matching rule; there is nothing to merge." };
  }
  if (left.serialNumber != null && right.serialNumber != null && left.serialNumber !== right.serialNumber) {
    return { outcome: "different", reason: "Both identities carry a serial number and the two differ, so they are separate units." };
  }
  if (seenTogether(left, right)) {
    return { outcome: "different", reason: "Both identities were enumerated in one scan, so they are two panels rather than two modes of one." };
  }
  return null;
}

/** Every unordered pair of the corpus, split into the ones code settles and
 *  the ones that need a judgment. */
export function candidatePairs(identities) {
  const settled = [];
  const open = [];
  for (let i = 0; i < identities.length; i += 1) {
    for (let j = i + 1; j < identities.length; j += 1) {
      const pair = { left: identities[i], right: identities[j] };
      const local = settleLocally(pair.left, pair.right);
      if (local) settled.push({ ...pair, ...local, decidedBy: "code" });
      else open.push(pair);
    }
  }
  return { settled, open };
}

// ---------------------------------------------------------------------------
// The judgment
// ---------------------------------------------------------------------------

/** What the model is told about one identity. Ids are for code and are left out. */
function describe(identity) {
  return {
    edidManufacturerId: identity.manufacturerId,
    edidProductCode: identity.productCode,
    edidProductName: identity.productName ?? null,
    serialNumberRead: identity.serialNumber ?? "none read from this display",
    resolutionWhenObserved: identity.observedResolution ?? null,
    operatingSystemThatRead: identity.observedOn ?? null,
  };
}

/**
 * What is actually on record about whether the two ever turned up together.
 *
 * This has to be said per pair rather than asserted once for all of them. The
 * blanket "these were never seen side by side" this function replaced was a
 * fabricated fact: it was sent for pairs whose identities came from captured
 * EDIDs with no simultaneity data at all, and it was the opposite of the truth
 * for a pair that one scan had listed together.
 */
function simultaneityFor(left, right) {
  const bothFromLiveScans = left.observedAlongside != null && right.observedAlongside != null;
  return bothFromLiveScans
    ? "Both identities came from scans of this computer's displays, and no scan has ever listed the two of them together."
    : "There is no record either way of whether these two identities have ever been enumerated at the same time.";
}

export function stateFor(left, right) {
  return {
    displayA: describe(left),
    displayB: describe(right),
    howTheseWereRead:
      "Each identity was read from the EDID of a display attached to the same computer. A display that is asleep or " +
      "showing another computer cannot be read at all, so a scan does not necessarily list every display. " +
      simultaneityFor(left, right),
  };
}

export function questionsFor() {
  return {
    relation: {
      type: "score",
      instructions:
        "Some displays publish a different EDID product code in each display mode, so one physical panel can appear under more than one identity; changing mode can also make a host read a serial number it could not read before. " +
        "Two units of the same model, and two models from one vendor, produce identities that look just as similar. " +
        "How do displayA and displayB relate?",
      criteria: [
        "Two different physical displays. They may share a vendor or even a product code, but they are separate panels, and treating them as one would move one panel's input settings onto the other.",
        "Impossible to settle from this evidence. The two could be one panel in two display modes, or two separate displays; the person who owns them has to say which.",
        "One physical display under two identities. The same panel publishes a different EDID product code depending on the display mode it is set to.",
      ],
    },
    sameProductLine: {
      type: "noul",
      instructions: "Do the two EDID product names name the same display model, or two members of one product line?",
      criteria: {
        true: "The names refer to the same model, or to one model written two ways by the same vendor.",
        false: "The names refer to different models, or one of them carries no usable name.",
      },
    },
    modeChangeExplainsDifference: {
      type: "noul",
      instructions:
        "Could every difference between displayA and displayB be explained by one physical panel being set to a different display mode?",
      criteria: {
        true: "Every difference is the kind a mode change produces on one panel: a different product code, a different reported resolution, a serial number appearing or disappearing.",
        false: "At least one difference is not something a mode change can produce on a single panel.",
      },
    },
    differentPanelSize: {
      type: "noul",
      instructions: "Do the two product names indicate panels of different physical screen sizes?",
      criteria: {
        true: "The names indicate different screen sizes, so they cannot be one panel.",
        false: "The names indicate the same size, or say nothing about size.",
      },
    },
  };
}

/** Rounds the Score to a level and applies the confidence gate. A pair the
 *  model calls one display without enough confidence becomes a question for the
 *  curator rather than a proposal — merging wrongly is the costly mistake. */
export function routeAnswer(answers, minConfidence = DEFAULT_MIN_CONFIDENCE) {
  const { score, confidence } = answers.relation;
  const level = Math.min(OUTCOMES.length - 1, Math.max(0, Math.round(score)));
  let outcome = OUTCOMES[level];
  let downgraded = false;
  if (outcome === "same" && confidence < minConfidence) {
    outcome = "ask";
    downgraded = true;
  }
  return {
    outcome,
    level,
    score,
    confidence,
    downgradedByConfidence: downgraded,
    reasons: {
      sameProductLine: answers.sameProductLine.noul,
      modeChangeExplainsDifference: answers.modeChangeExplainsDifference.noul,
      differentPanelSize: answers.differentPanelSize.noul,
    },
  };
}

// ---------------------------------------------------------------------------
// Importing a scan into the corpus
// ---------------------------------------------------------------------------

const slug = (value) => String(value ?? "").trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");

/** A deterministic corpus id, so re-importing a display lands on its entry. */
export function corpusIdFor(fingerprint) {
  const parts = [fingerprint.manufacturer_id, fingerprint.product_code, fingerprint.serial_number];
  return parts.filter(Boolean).map(slug).filter(Boolean).join("-");
}

/** Which host read this scan, from the ids MuxSU builds per platform. */
function platformOf(monitors) {
  const id = monitors.find((monitor) => typeof monitor.id === "string")?.id ?? "";
  if (id.startsWith("macos:")) return "macOS";
  if (id.startsWith("windows:") || /^\\\\[?.]\\/.test(id) || id.toUpperCase().startsWith("DISPLAY")) return "Windows";
  return null;
}

function serialSourceFor(serial, platform) {
  if (serial == null) return "no serial read on this host";
  if (platform === "Windows") return "EDID serial text, as Windows WMI SerialNumberID reads it";
  if (platform === "macOS") return "EDID 32-bit numeric serial, as macOS reads it";
  return "serial as this host read it";
}

/**
 * The corpus with one `muxsu-cli list --json` scan folded in.
 *
 * Entries are matched on the *exact* fingerprint, serial included, so a host
 * that reads a serial another host cannot gets its own entry rather than
 * overwriting the other's. That difference is the very thing the oracle needs to
 * see: the MSI MPG 274U's two product codes carrying one identical serial on
 * Windows is decisive evidence, where the same two codes with no serial at all
 * on macOS are not.
 *
 * Built-in panels are skipped; they are never shared between computers.
 */
export function importScan(corpus, monitors, note) {
  if (!note) throw new Error("import needs --note describing how the scan was taken; the corpus records provenance for every entry.");
  if (!Array.isArray(monitors)) throw new Error("a scan must be the JSON array that `muxsu-cli list --json` prints.");
  const platform = platformOf(monitors);
  const identities = corpus.identities.map((entry) => ({ ...entry }));
  const external = monitors.filter((monitor) => monitor.builtIn !== true && monitor.fingerprint);

  const touched = external.map((monitor) => {
    const { fingerprint } = monitor;
    const existing = identities.find(
      (entry) =>
        normalize(entry.manufacturerId) === normalize(fingerprint.manufacturer_id) &&
        normalize(entry.productCode) === normalize(fingerprint.product_code) &&
        (entry.serialNumber ?? null) === (fingerprint.serial_number ?? null),
    );
    if (existing) return existing;
    const added = {
      id: corpusIdFor(fingerprint),
      manufacturerId: fingerprint.manufacturer_id,
      productCode: fingerprint.product_code,
      productName: monitor.name ?? null,
      serialNumber: fingerprint.serial_number ?? null,
      serialSource: serialSourceFor(fingerprint.serial_number ?? null, platform),
      observedResolution: monitor.maxResolution
        ? `${monitor.maxResolution.width}x${monitor.maxResolution.height}`
        : null,
      observedOn: platform,
      provenance: note,
    };
    identities.push(added);
    return added;
  });

  // Everything one scan listed was enumerated together, which is what makes
  // these two panels rather than two modes of one. Earlier observations are kept:
  // having ever been seen together is enough.
  for (const entry of touched) {
    const others = touched.filter((other) => other !== entry).map((other) => other.id);
    entry.observedAlongside = [...new Set([...(entry.observedAlongside ?? []), ...others])];
  }
  return { corpus: { ...corpus, identities }, added: identities.length - corpus.identities.length, scanned: touched.length, platform };
}

// ---------------------------------------------------------------------------
// Promotion into the shipped table
// ---------------------------------------------------------------------------

const identityKey = (identity) => `${normalize(identity.manufacturerId)}:${normalize(identity.productCode)}`;

/**
 * The shipped table with every curator-approved proposal folded in. A proposal
 * counts as approved only once a human has added `"approved": true` to it by
 * hand; the model's own verdict never promotes itself.
 *
 * A group is keyed by vendor and product code alone, because "these two product
 * codes are one panel" is a fact about the model, not about one person's unit.
 * That is also its limit: someone who owns two of the same display, one per
 * mode, would be offered a merge of two genuinely separate panels — which is
 * why the table only ever preselects, and the user still declares.
 */
export function promote(table, proposals) {
  const groups = (table.groups ?? []).map((group) => ({ ...group, identities: [...group.identities] }));
  const findGroup = (key) => groups.find((group) => group.identities.some((identity) => identityKey(identity) === key));
  let added = 0;

  for (const proposal of proposals.proposals ?? []) {
    if (proposal.approved !== true || proposal.outcome !== "same") continue;
    const identities = [proposal.left, proposal.right];
    const keys = identities.map(identityKey);
    if (keys[0] === keys[1]) continue;
    const existing = findGroup(keys[0]) ?? findGroup(keys[1]);
    if (existing) {
      for (const [index, key] of keys.entries()) {
        if (!existing.identities.some((identity) => identityKey(identity) === key)) {
          existing.identities.push(entryFor(identities[index]));
          added += 1;
        }
      }
      continue;
    }
    groups.push({
      panel: proposal.left.productName ?? keys[0],
      identities: identities.map(entryFor),
      source: "curator",
      evidence: proposal.evidence ?? `Approved from ${DEFAULT_PROPOSALS}; Jev score ${proposal.score}, confidence ${proposal.confidence}.`,
    });
    added += 2;
  }
  return { table: { ...table, groups }, added };
}

const entryFor = (identity) => ({
  manufacturerId: normalize(identity.manufacturerId),
  productCode: normalize(identity.productCode),
});

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

// `resolve` rather than `join`, so a path outside the repo — the scan file a
// person just captured, say — is taken as given instead of appended to the root.
/**
 * Text from a file whatever encoded it, by the byte-order mark it left.
 *
 * Every file this tool reads is one a person produced a moment earlier, and on
 * Windows that means an encoding nobody chose: `>` in Windows PowerShell 5.1
 * redirects to UTF-16LE, and Notepad writes UTF-8 with a mark on the front.
 * Reading those as plain UTF-8 fails on the first character, which says nothing
 * about what went wrong. Decode by the mark instead and the scan a person just
 * captured simply works.
 */
export function decodeText(bytes) {
  if (bytes.length >= 2 && bytes[0] === 0xff && bytes[1] === 0xfe) {
    return bytes.subarray(2).toString("utf16le");
  }
  if (bytes.length >= 2 && bytes[0] === 0xfe && bytes[1] === 0xff) {
    // Node decodes UTF-16 little-endian only, so swap the pairs first.
    return Buffer.from(bytes.subarray(2)).swap16().toString("utf16le");
  }
  if (bytes.length >= 3 && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf) {
    return bytes.subarray(3).toString("utf8");
  }
  return bytes.toString("utf8");
}

const readJson = (path) => JSON.parse(decodeText(readFileSync(resolve(root, path))));
// Written as UTF-8 without a mark, so the corpus stays diffable wherever it is
// regenerated.
const writeJson = (path, value) => writeFileSync(resolve(root, path), `${JSON.stringify(value, null, 2)}\n`, "utf8");

function argument(name, fallback) {
  const index = process.argv.indexOf(name);
  if (index < 0) return fallback;
  const value = process.argv[index + 1];
  if (!value) throw new Error(`${name} needs a value`);
  return value;
}

const cacheKey = (payload) => createHash("sha256").update(JSON.stringify(payload)).digest("hex").slice(0, 32);

async function propose() {
  const corpusPath = argument("--corpus", DEFAULT_CORPUS);
  const cachePath = argument("--cache", DEFAULT_CACHE);
  const minConfidence = Number(argument("--confidence", String(DEFAULT_MIN_CONFIDENCE)));
  const model = argument("--model", DEFAULT_MODEL);
  const { identities } = readJson(corpusPath);
  const { settled, open } = candidatePairs(identities);
  const cache = existsSync(resolve(root, cachePath)) ? readJson(cachePath) : {};

  console.log(`${identities.length} identities: ${settled.length} pair(s) settled in code, ${open.length} to judge.`);

  let client = null;
  const proposals = [];
  for (const pair of open) {
    const payload = { model, state: stateFor(pair.left, pair.right), questions: questionsFor() };
    const key = cacheKey(payload);
    let answers = cache[key];
    if (!answers) {
      if (!client) {
        if (!process.env.TYPESAFE_API_KEY) {
          throw new Error("TYPESAFE_API_KEY is not set, and this pair is not cached. Set the key or run with a warm cache.");
        }
        const { TypeSafeClient } = await import("@typesafe-ai/sdk");
        client = new TypeSafeClient();
      }
      const result = await client.systemOne(payload);
      answers = result.answers;
      cache[key] = answers;
      writeJson(cachePath, cache);
      console.log(`  asked ${model} about ${pair.left.id} vs ${pair.right.id} (${result.usage.input_tokens} input tokens)`);
    }
    const routed = routeAnswer(answers, minConfidence);
    proposals.push({ left: pair.left, right: pair.right, ...routed, approved: null });
  }

  const counts = Object.fromEntries(OUTCOMES.map((outcome) => [outcome, proposals.filter((p) => p.outcome === outcome).length]));
  writeJson(argument("--out", DEFAULT_PROPOSALS), {
    note: `Written by scripts/identity-oracle.mjs. Review each proposal and set "approved": true on the ones you accept, then run: node scripts/identity-oracle.mjs promote`,
    model,
    minConfidence,
    generatedAt: new Date().toISOString(),
    settledInCode: settled,
    proposals,
  });
  console.log(`judged ${proposals.length}: ${JSON.stringify(counts)} -> ${argument("--out", DEFAULT_PROPOSALS)}`);
}

function importCommand() {
  const corpusPath = argument("--corpus", DEFAULT_CORPUS);
  const scanPath = argument("--scan", null);
  const scan = scanPath ? readJson(scanPath) : JSON.parse(decodeText(readFileSync(0)));
  const result = importScan(readJson(corpusPath), scan, argument("--note", null));
  writeJson(corpusPath, result.corpus);
  console.log(
    `${result.platform ?? "unknown host"}: ${result.scanned} external display(s) in the scan, ${result.added} new to ${corpusPath}.`,
  );
}

function promoteCommand() {
  const tablePath = argument("--table", DEFAULT_TABLE);
  const { table, added } = promote(readJson(tablePath), readJson(argument("--proposals", DEFAULT_PROPOSALS)));
  if (!added) {
    console.log('Nothing approved. Add "approved": true to a proposal you accept first.');
    return;
  }
  writeJson(tablePath, table);
  console.log(`added ${added} identit${added === 1 ? "y" : "ies"} to ${tablePath}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const command = process.argv[2];
  try {
    if (command === "propose") await propose();
    else if (command === "promote") promoteCommand();
    else if (command === "import") importCommand();
    else {
      console.error(
        "usage: node scripts/identity-oracle.mjs <import|propose|promote>\n" +
          "  import   --note <text> [--scan file.json | stdin] [--corpus p]\n" +
          "  propose  [--corpus p] [--out p] [--cache p] [--model m] [--confidence n]\n" +
          "  promote  [--proposals p] [--table p]",
      );
      process.exit(1);
    }
  } catch (error) {
    // A missing key or an API failure is an ordinary outcome for a dev tool;
    // report it in one line rather than a stack trace.
    console.error(`identity-oracle: ${error instanceof Error ? error.message : error}`);
    process.exit(1);
  }
}
