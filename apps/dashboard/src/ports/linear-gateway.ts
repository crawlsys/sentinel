// SENTINEL-23 — Linear gateway port.
//
// Read-side access to Linear data (tickets + teams). Adapters wrap the
// Linear MCP / REST API behind this contract so the application layer
// stays Linear-implementation-agnostic.
//
// Pure interface declarations only — zero implementations, zero IO.

import type { Ticket, Team } from "../domain";
import type { TimeRange } from "./time-range";

export interface LinearGateway {
  getActiveTickets(): Promise<Ticket[]>;
  getCompletedTickets(window: TimeRange): Promise<Ticket[]>;
  getTeams(): Promise<Team[]>;
}
