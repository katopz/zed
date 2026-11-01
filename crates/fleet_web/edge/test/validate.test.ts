import assert from "node:assert/strict";
import { test } from "node:test";
import { isValidRig, isValidThreadId, threadApiPath } from "../src/validate.ts";

test("rig names", () => {
  for (const rig of ["m3", "amo", "4090", "rig.lan", "my_rig-2"]) assert.ok(isValidRig(rig), rig);
  for (const rig of ["", ".", "..", "a/b", "a%2Fb", "a b", "x".repeat(65), "rig?x=1", "ríg"]) {
    assert.ok(!isValidRig(rig), JSON.stringify(rig));
  }
});

test("thread ids", () => {
  assert.ok(isValidThreadId("0195f0e2-7a1c-7b3e-9d2a-1f2e3d4c5b6a"));
  for (const id of ["", "..", "a.b", "a/b", "a_b", "x".repeat(65), "%2e%2e"]) assert.ok(!isValidThreadId(id), id);
});

test("thread api path only for valid segments", () => {
  assert.equal(threadApiPath("m3", "abc-123"), "/api/threads/m3/abc-123");
  assert.equal(threadApiPath("..", "abc"), null);
  assert.equal(threadApiPath("m3", "../../etc"), null);
});
