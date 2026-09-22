const storageKey = "muxsu.experimental";

/**
 * Whether the display actions that write to a display nobody has verified
 * this on are offered at all. Off until the user turns it on: the ones behind
 * it can leave a display showing another computer, and a setting the user has
 * never seen is not consent.
 *
 * Kept on this computer rather than in the settings file. It decides what this
 * window offers, not what any display or paired host does, so there is nothing
 * for another host to learn from it.
 */
export function experimentalEnabled(): boolean {
  try {
    return localStorage.getItem(storageKey) === "on";
  } catch {
    return false;
  }
}

/** Returns what the preference now reads, which is unchanged when it cannot be stored. */
export function setExperimentalEnabled(enabled: boolean): boolean {
  try {
    if (enabled) localStorage.setItem(storageKey, "on");
    else localStorage.removeItem(storageKey);
  } catch {
    return experimentalEnabled();
  }
  return enabled;
}
