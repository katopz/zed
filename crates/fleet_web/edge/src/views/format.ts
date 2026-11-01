export function toMillis(value: string | number | null | undefined): number | null {
  if (value === null || value === undefined || value === "") return null;
  if (typeof value === "number") return value < 1e12 ? value * 1000 : value;
  if (/^\d+$/.test(value)) return toMillis(Number(value));
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : parsed;
}

export function relativeTime(value: string | number | null | undefined, now: number): string {
  const millis = toMillis(value);
  if (millis === null) return "unknown";
  const seconds = Math.round((now - millis) / 1000);
  if (seconds < 0) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

export function absoluteTime(value: string | number | null | undefined): string {
  const millis = toMillis(value);
  return millis === null ? "" : new Date(millis).toISOString().replace("T", " ").slice(0, 16) + " UTC";
}

export function basename(path: string): string {
  const parts = path.split(/[\\/]/).filter((part) => part.length > 0);
  return parts.length > 0 ? parts[parts.length - 1] : path;
}

export function clip(text: string, maxChars: number): string {
  const chars = Array.from(text);
  return chars.length <= maxChars ? text : chars.slice(0, maxChars).join("") + "…";
}

export function compactNumber(value: number): string {
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  if (value >= 1e3) return `${(value / 1e3).toFixed(1)}k`;
  return String(value);
}

export function fixed(value: number | null | undefined, digits = 1): string {
  return typeof value === "number" && Number.isFinite(value) ? value.toFixed(digits) : "?";
}
