import { describe, it, expect } from "vitest";
import { SegmentedBar } from "@/components/atoms/SegmentedBar";
import { renderWithTheme } from "./test-utils";

function countFilled(container: HTMLElement): number {
  return container.querySelectorAll('[data-filled="true"]').length;
}

function countSegments(container: HTMLElement): number {
  const bar = container.querySelector('[data-testid="segmented-bar"]');
  if (!bar) throw new Error("segmented-bar element not found");
  // Total segments = direct children of the bar.
  return bar.children.length;
}

describe("SegmentedBar", () => {
  it("renders without crashing in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <SegmentedBar value={0.5} />,
      "dark",
    );
    expect(container.querySelector('[data-testid="segmented-bar"]')).not.toBeNull();
    unmount();
  });

  it("renders without crashing in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <SegmentedBar value={0.5} />,
      "light",
    );
    expect(container.querySelector('[data-testid="segmented-bar"]')).not.toBeNull();
    unmount();
  });

  it("matches snapshot in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <SegmentedBar value={0.6} segments={10} tone="success" />,
      "dark",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <SegmentedBar value={0.6} segments={10} tone="success" />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("value=0 fills 0 of 20 segments", () => {
    const { container, unmount } = renderWithTheme(
      <SegmentedBar value={0} />,
      "dark",
    );
    expect(countSegments(container)).toBe(20);
    expect(countFilled(container)).toBe(0);
    unmount();
  });

  it("value=1 fills all 20 of 20 segments", () => {
    const { container, unmount } = renderWithTheme(
      <SegmentedBar value={1} />,
      "dark",
    );
    expect(countSegments(container)).toBe(20);
    expect(countFilled(container)).toBe(20);
    unmount();
  });

  it("value=0.5 fills 10 of 20 segments", () => {
    const { container, unmount } = renderWithTheme(
      <SegmentedBar value={0.5} />,
      "dark",
    );
    expect(countSegments(container)).toBe(20);
    expect(countFilled(container)).toBe(10);
    unmount();
  });

  it("clamps values outside [0, 1]", () => {
    const negative = renderWithTheme(<SegmentedBar value={-0.5} />, "dark");
    const over = renderWithTheme(<SegmentedBar value={1.5} />, "dark");
    expect(countFilled(negative.container)).toBe(0);
    expect(countFilled(over.container)).toBe(20);
    negative.unmount();
    over.unmount();
  });

  it("respects custom segment count", () => {
    const { container, unmount } = renderWithTheme(
      <SegmentedBar value={0.5} segments={8} />,
      "dark",
    );
    expect(countSegments(container)).toBe(8);
    expect(countFilled(container)).toBe(4);
    unmount();
  });
});
