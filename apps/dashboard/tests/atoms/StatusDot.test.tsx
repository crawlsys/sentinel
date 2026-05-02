import { describe, it, expect } from "vitest";
import { StatusDot } from "@/components/atoms/StatusDot";
import { renderWithTheme } from "./test-utils";

function backgroundOf(container: HTMLElement): string {
  const dot = container.querySelector<HTMLElement>('[data-testid="status-dot"]');
  if (!dot) throw new Error("status-dot element not found");
  // We read the inline element styles via getComputedStyle. jsdom resolves
  // inline styles even though it doesn't run a full layout engine.
  return getComputedStyle(dot).backgroundColor || dot.style.backgroundColor;
}

describe("StatusDot", () => {
  it("renders without crashing in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <StatusDot tone="success" />,
      "dark",
    );
    expect(container.querySelector('[data-testid="status-dot"]')).not.toBeNull();
    unmount();
  });

  it("renders without crashing in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <StatusDot tone="success" />,
      "light",
    );
    expect(container.querySelector('[data-testid="status-dot"]')).not.toBeNull();
    unmount();
  });

  it("matches snapshot in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <StatusDot tone="error" />,
      "dark",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <StatusDot tone="error" />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("the four tones map to four distinct colors (dark mode)", () => {
    const successRender = renderWithTheme(<StatusDot tone="success" />, "dark");
    const warnRender = renderWithTheme(<StatusDot tone="warn" />, "dark");
    const errorRender = renderWithTheme(<StatusDot tone="error" />, "dark");
    const idleRender = renderWithTheme(<StatusDot tone="idle" />, "dark");

    const colors = new Set([
      backgroundOf(successRender.container),
      backgroundOf(warnRender.container),
      backgroundOf(errorRender.container),
      backgroundOf(idleRender.container),
    ]);

    // All four tones should resolve to four distinct background colors.
    expect(colors.size).toBe(4);

    successRender.unmount();
    warnRender.unmount();
    errorRender.unmount();
    idleRender.unmount();
  });

  it("error tone uses Nothing accent red", () => {
    const { container, unmount } = renderWithTheme(
      <StatusDot tone="error" />,
      "dark",
    );
    const bg = backgroundOf(container).toLowerCase();
    // #D71921 → rgb(215, 25, 33). Accept either string form.
    expect(
      bg.includes("215, 25, 33") ||
        bg.includes("#d71921") ||
        bg.includes("215,25,33"),
    ).toBe(true);
    unmount();
  });
});
