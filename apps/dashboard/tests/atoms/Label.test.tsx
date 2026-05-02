import { describe, it, expect } from "vitest";
import { Label } from "@/components/atoms/Label";
import { renderWithTheme } from "./test-utils";

describe("Label", () => {
  it("renders without crashing in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <Label>Latency</Label>,
      "dark",
    );
    expect(container.textContent).toBe("Latency");
    unmount();
  });

  it("renders without crashing in light mode", () => {
    const { container, unmount } = renderWithTheme(
      <Label>Latency</Label>,
      "light",
    );
    expect(container.textContent).toBe("Latency");
    unmount();
  });

  it("matches snapshot in dark mode (primary tone)", () => {
    const { container, unmount } = renderWithTheme(
      <Label tone="primary">DEPLOY</Label>,
      "dark",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("matches snapshot in light mode (primary tone)", () => {
    const { container, unmount } = renderWithTheme(
      <Label tone="primary">DEPLOY</Label>,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });

  it("supports secondary and disabled tones", () => {
    const { container: a, unmount: u1 } = renderWithTheme(
      <Label tone="secondary">SECONDARY</Label>,
      "dark",
    );
    const { container: b, unmount: u2 } = renderWithTheme(
      <Label tone="disabled">DISABLED</Label>,
      "dark",
    );
    expect(a.textContent).toBe("SECONDARY");
    expect(b.textContent).toBe("DISABLED");
    u1();
    u2();
  });
});
