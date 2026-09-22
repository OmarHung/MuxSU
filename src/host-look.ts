import {
  BriefcaseBusiness, Clapperboard, CodeXml, Computer, Gamepad2, GraduationCap, House, Laptop, PcCase, Server,
  Tablet, Tv,
} from "lucide";
import type { MessageKey } from "./i18n";

/** How a host is drawn: an icon and a colour, each a name the backend
 *  validates (see `host_appearance.rs`). Shared by the main window and the
 *  host switcher so a host looks the same in both. */

type Platform = "windows" | "mac";

/** Icon names, in picker order, with the Lucide drawing each stands for. */
export const HOST_ICONS = [
  { key: "desktop", lucide: "computer", label: "hostLook.icon.desktop" },
  { key: "laptop", lucide: "laptop", label: "hostLook.icon.laptop" },
  { key: "tower", lucide: "pc-case", label: "hostLook.icon.tower" },
  { key: "server", lucide: "server", label: "hostLook.icon.server" },
  { key: "gamepad", lucide: "gamepad-2", label: "hostLook.icon.gamepad" },
  { key: "tablet", lucide: "tablet", label: "hostLook.icon.tablet" },
  { key: "tv", lucide: "tv", label: "hostLook.icon.tv" },
  { key: "work", lucide: "briefcase-business", label: "hostLook.icon.work" },
  { key: "home", lucide: "house", label: "hostLook.icon.home" },
  { key: "code", lucide: "code-xml", label: "hostLook.icon.code" },
  { key: "media", lucide: "clapperboard", label: "hostLook.icon.media" },
  { key: "school", lucide: "graduation-cap", label: "hostLook.icon.school" },
] as const satisfies readonly { key: string; lucide: string; label: MessageKey }[];

/** Colour names, in picker order; tokens.css gives each a light and dark shade. */
export const HOST_COLORS = [
  { key: "indigo", label: "hostLook.color.indigo" },
  { key: "teal", label: "hostLook.color.teal" },
  { key: "pink", label: "hostLook.color.pink" },
  { key: "blue", label: "hostLook.color.blue" },
  { key: "orange", label: "hostLook.color.orange" },
  { key: "green", label: "hostLook.color.green" },
  { key: "purple", label: "hostLook.color.purple" },
  { key: "red", label: "hostLook.color.red" },
  { key: "yellow", label: "hostLook.color.yellow" },
  { key: "graphite", label: "hostLook.color.graphite" },
  { key: "black", label: "hostLook.color.black" },
  { key: "silver", label: "hostLook.color.silver" },
] as const satisfies readonly { key: string; label: MessageKey }[];

/** Colours hosts wear by default, by their place in the host order. Paired
 *  hosts share that order, so a host defaults to the same colour everywhere. */
const DEFAULT_COLORS = ["indigo", "teal", "pink", "blue", "orange"] as const;

/** Every drawing a host icon can use, for `createIcons`. */
export const hostIconSet = {
  BriefcaseBusiness, Clapperboard, CodeXml, Computer, Gamepad2, GraduationCap, House, Laptop, PcCase, Server, Tablet, Tv,
};

/** A custom look as the backend reports it; missing parts use the default. */
export interface CustomLook { icon?: string | null; color?: string | null }

export interface HostLook {
  /** Lucide name to draw. */
  lucide: string;
  /** Colour name for `data-color`. */
  color: string;
  /** The chosen icon name, or null when it is the default. */
  customIcon: string | null;
  /** The chosen colour name, or null when it is the default. */
  customColor: string | null;
}

function knownIcon(key: string | null | undefined) {
  return HOST_ICONS.find((icon) => icon.key === key);
}

function knownColor(key: string | null | undefined) {
  return HOST_COLORS.find((color) => color.key === key);
}

/** How a host looks: its custom icon and colour where set and known, else a
 *  laptop or desktop by platform and a colour by its place in the host order. */
export function hostLook(custom: CustomLook | undefined, platform: Platform, orderIndex: number): HostLook {
  const icon = knownIcon(custom?.icon);
  const color = knownColor(custom?.color);
  return {
    lucide: icon?.lucide ?? (platform === "mac" ? "laptop" : "computer"),
    color: color?.key ?? DEFAULT_COLORS[Math.max(orderIndex, 0) % DEFAULT_COLORS.length],
    customIcon: icon?.key ?? null,
    customColor: color?.key ?? null,
  };
}
