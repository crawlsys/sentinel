import { describe, it, expect } from "vitest";

import { DashboardLayout } from "@/components/templates/DashboardLayout";
import { renderWithTheme } from "../atoms/test-utils";

describe("DashboardLayout", () => {
  it("renders SENTINEL header + content slot in dark mode", () => {
    const { container, unmount } = renderWithTheme(
      <DashboardLayout>
        <div data-testid="content-marker">hello</div>
      </DashboardLayout>,
      "dark",
    );
    expect(container.textContent).toContain("SENTINEL");
    expect(
      container.querySelector('[data-testid="dashboard-content"]'),
    ).not.toBeNull();
    expect(
      container.querySelector('[data-testid="content-marker"]'),
    ).not.toBeNull();
    unmount();
  });

  it("renders the windowLabel tag when provided", () => {
    const { container, unmount } = renderWithTheme(
      <DashboardLayout windowLabel="LAST 30 DAYS">
        <span />
      </DashboardLayout>,
      "dark",
    );
    expect(container.textContent).toContain("LAST 30 DAYS");
    unmount();
  });

  it("omits the windowLabel tag when not provided", () => {
    const { container, unmount } = renderWithTheme(
      <DashboardLayout>
        <span />
      </DashboardLayout>,
      "dark",
    );
    // WINDOW label is always there, but the Tag element should be absent.
    expect(container.querySelectorAll('[data-testid="tag"]').length).toBe(0);
    unmount();
  });

  it("renders without crashing in light mode and matches snapshot", () => {
    const { container, unmount } = renderWithTheme(
      <DashboardLayout windowLabel="LAST 7 DAYS">
        <div>body</div>
      </DashboardLayout>,
      "light",
    );
    expect(container).toMatchSnapshot();
    unmount();
  });
});
