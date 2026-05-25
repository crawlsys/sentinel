import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";

function App({ events = [] as Array<{ id: string; label: string }> }) {
  return (
    <main>
      <h1>sentinel-viz-next</h1>
      <ul data-testid="events">
        {events.map((e) => (
          <li key={e.id}>{e.label}</li>
        ))}
      </ul>
    </main>
  );
}

describe("App smoke", () => {
  it("renders without crashing on empty data", () => {
    render(<App />);
    expect(screen.getByRole("heading", { name: /sentinel-viz-next/i })).toBeInTheDocument();
    expect(screen.getByTestId("events").children).toHaveLength(0);
  });

  it("renders supplied events", () => {
    render(<App events={[{ id: "1", label: "first" }]} />);
    expect(screen.getByText("first")).toBeInTheDocument();
  });
});
