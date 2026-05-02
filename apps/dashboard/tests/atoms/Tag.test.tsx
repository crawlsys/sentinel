import { describe, it, expect } from "vitest";
import { Tag } from "@/components/atoms/Tag";
import { renderWithTheme } from "./test-utils";

describe("Tag", () => {
  it("renders without crashing in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <Tag>Active</Tag>,
      "dark",
    );
    expect(container.textContent).toBe("Active");
    unmount();
  });

  it("renders without crashing in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <Tag>Active</Tag>,
      "light",
    );
    expect(container.textContent).toBe("Active");
    unmount();
  });

  it("matches snapshot in dark mode (default tone)", () => {
    const { container, unmount } = renderWithTheme(
      <Tag tone="default">PROD</Tag>,
      "dark",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("matches snapshot in light mode (default tone)", () => {
    const { container, unmount } = renderWithTheme(
      <Tag tone="default">PROD</Tag>,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("supports success / warn / error tones", () => {
    const tones = ["success", "warn", "error"] as const;
    for (const tone of tones) {
      const { container, unmount } = renderWithTheme(
        <Tag tone={tone}>{tone.toUpperCase()}</Tag>,
        "dark",
      );
      expect(container.textContent).toBe(tone.toUpperCase());
      unmount();
    }
  });

  it("uppercase=false preserves casing", () => {
    const { container, unmount } = renderWithTheme(
      <Tag uppercase={false}>MixedCase</Tag>,
      "dark",
    );
    expect(container.textContent).toBe("MixedCase");
    unmount();
  });
});
