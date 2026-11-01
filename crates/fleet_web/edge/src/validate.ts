export const RIG_PATTERN = /^[A-Za-z0-9._-]{1,64}$/;
export const THREAD_ID_PATTERN = /^[A-Za-z0-9-]{1,64}$/;

export function isValidRig(rig: string): boolean {
  // `.` and `..` match the character class but are path traversal, not rig names.
  return RIG_PATTERN.test(rig) && rig !== "." && rig !== "..";
}

export function isValidThreadId(id: string): boolean {
  return THREAD_ID_PATTERN.test(id);
}

/** Origin path for one thread, or null when either segment is invalid. */
export function threadApiPath(rig: string, id: string): string | null {
  if (!isValidRig(rig) || !isValidThreadId(id)) return null;
  return `/api/threads/${encodeURIComponent(rig)}/${encodeURIComponent(id)}`;
}
