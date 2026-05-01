"use client";

import { useState } from "react";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Chip from "@mui/material/Chip";
import Stack from "@mui/material/Stack";
import Tab from "@mui/material/Tab";
import Tabs from "@mui/material/Tabs";
import TextField from "@mui/material/TextField";
import Typography from "@mui/material/Typography";

/**
 * Theme conformance sample page (SENTINEL-21).
 *
 * Renders a minimal collection of MUI primitives so visual inspection can
 * confirm the Nothing theme is wired correctly: Doto hero, mono labels,
 * pill buttons, flat card, square text inputs, sharp tab indicator.
 */
export default function Home() {
  const [tab, setTab] = useState(0);

  return (
    <Box
      component="main"
      sx={{
        minHeight: "100vh",
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "flex-start",
        px: 4,
        py: 8,
        gap: 4,
      }}
    >
      <Typography variant="h1" component="h1">
        SENTINEL
      </Typography>
      <Typography variant="overline" color="text.secondary">
        Hook Engine Dashboard
      </Typography>

      <Card sx={{ width: "100%", maxWidth: 640 }}>
        <CardContent>
          <Stack spacing={3}>
            <Box>
              <Typography variant="overline" color="text.secondary">
                Status
              </Typography>
              <Stack direction="row" spacing={1} sx={{ mt: 1 }}>
                <Chip label="Active" color="error" />
                <Chip label="56 Hooks" variant="outlined" />
                <Chip label="77 Skills" variant="outlined" />
              </Stack>
            </Box>

            <TextField
              label="Session ID"
              defaultValue="claude-1299400-1777565103231"
              fullWidth
            />

            <Stack direction="row" spacing={2}>
              <Button>Verify Chain</Button>
              <Button color="error">Abort</Button>
              <Button variant="contained">Submit</Button>
            </Stack>

            <Box>
              <Tabs
                value={tab}
                onChange={(_, v: number) => setTab(v)}
                aria-label="dashboard sections"
              >
                <Tab label="Proofs" />
                <Tab label="Workflows" />
                <Tab label="Metrics" />
                <Tab label="Telemetry" />
              </Tabs>
              <Typography variant="caption" sx={{ display: "block", mt: 2 }}>
                Selected tab index: {tab}
              </Typography>
            </Box>
          </Stack>
        </CardContent>
      </Card>
    </Box>
  );
}
