import { describe, it, expect } from "vitest";
import { WipChip } from "@/components/molecules/WipChip";
import { renderWithTheme } from "../atoms/test-utils";

describe("WipChip", () => {
  it("renders stage + count in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <WipChip stage="Code Review" count={3} />,
      "dark",
    );
    expect(container.textContent).toContain("Code Review: 3");
    unmount();
  });

  it("uses success tone when no threshold is supplied", () => {
    const { container, unmount } = renderWithTheme(
      <WipChip stage="In Progress" count={100} />,
      "dark",
    );
    expect(
      container.querySelector('[data-testid="status-dot"]')?.getAttribute("data-tone"),
    ).toBe("success");
    unmount();
  });

  it("flips to warn when count crosses 75% of threshold", () => {
    const { container, unmount } = renderWithTheme(
      <WipChip stage="In Progress" count={8} threshold={10} />,
      "dark",
    );
    expect(
      container.querySelector('[data-testid="status-dot"]')?.getAttribute("data-tone"),
    ).toBe("warn");
    unmount();
  });

  it("flips to error when count meets or exceeds threshold", () => {
    const { container, unmount } = renderWithTheme(
      <WipChip stage="In Progress" count={12} threshold={10} />,
      "dark",
    );
    expect(
      container.querySelector('[data-testid="status-dot"]')?.getAttribute("data-tone"),
    ).toBe("error");
    unmount();
  });

  it("matches snapshot in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <WipChip stage="QA Testing" count={5} threshold={4} />,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
