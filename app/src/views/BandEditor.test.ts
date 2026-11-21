import { expect, it } from "vitest";
import { guardEditorChrome } from "./BandEditor";

function ev(type: string, tagName: string, detail = 1) {
  const e = { type, detail, target: { tagName }, prevented: false, preventDefault() { e.prevented = true; } };
  return e;
}

it("blocks a press after a double-click on the editor's chrome", () => {
  for (const tag of ["DIV", "BUTTON", "svg", "SPAN"]) {
    const e = ev("mousedown", tag, 2);
    guardEditorChrome(e);
    expect(e.prevented, tag).toBe(true);
  }
});

it("leaves single clicks alone, so buttons still take focus and fields commit", () => {
  const e = ev("mousedown", "BUTTON", 1);
  guardEditorChrome(e);
  expect(e.prevented).toBe(false);
});

it("blocks native drags from the chrome", () => {
  const e = ev("dragstart", "BUTTON");
  guardEditorChrome(e);
  expect(e.prevented).toBe(true);
});

it("never touches the text fields", () => {
  for (const e of [ev("mousedown", "INPUT", 2), ev("mousedown", "INPUT", 3), ev("dragstart", "INPUT")]) {
    guardEditorChrome(e);
    expect(e.prevented).toBe(false);
  }
});
