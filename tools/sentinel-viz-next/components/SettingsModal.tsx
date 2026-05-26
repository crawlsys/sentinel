"use client";

import { useEffect, useState } from "react";
import {
  Box,
  Button,
  Dialog,
  DialogActions,
  DialogContent,
  Divider,
  IconButton,
  MenuItem,
  Select,
  Stack,
  TextField,
  Typography,
} from "@mui/material";
import CloseIcon from "@mui/icons-material/CloseRounded";

import { fetchConfig, setConfig } from "../adapters/http";

interface Props {
  open: boolean;
  onClose: () => void;
}

const MODEL_PRESETS = [
  { value: "none", label: "Disabled (UUID fallback)" },
  { value: "openrouter:openai/gpt-4o-mini", label: "OpenRouter · gpt-4o-mini (cheap+good, default)" },
  { value: "openrouter:google/gemini-2.0-flash-001", label: "OpenRouter · gemini-2.0-flash (very cheap)" },
  { value: "openrouter:anthropic/claude-3.5-haiku", label: "OpenRouter · claude-3.5-haiku" },
  { value: "openrouter:openai/gpt-4o", label: "OpenRouter · gpt-4o (stronger, costlier)" },
  { value: "openai:gpt-4o-mini", label: "OpenAI direct · gpt-4o-mini" },
  { value: "openai:gpt-4o", label: "OpenAI direct · gpt-4o" },
  { value: "local:qwen2.5:1.5b", label: "Local · qwen2.5:1.5b (fast, private)" },
  { value: "local:qwen2.5-coder:7b", label: "Local · qwen2.5-coder:7b" },
];

const LABEL_SX = {
  fontFamily: "var(--font-space-mono), monospace",
  fontSize: 10,
  letterSpacing: "0.08em",
  textTransform: "uppercase",
  color: "var(--text-secondary)",
};

const HINT_SX = {
  fontFamily: "var(--font-space-mono), monospace",
  fontSize: 10,
  color: "var(--text-secondary)",
  mt: 0.5,
};

export function SettingsModal({ open, onClose }: Props) {
  const [currentModel, setCurrentModel] = useState<string>("none");
  const [currentHasKey, setCurrentHasKey] = useState<boolean>(false);
  const [pendingModel, setPendingModel] = useState<string>("none");
  const [pendingKey, setPendingKey] = useState<string>("");
  const [pendingOllama, setPendingOllama] = useState<string>("");
  const [saving, setSaving] = useState<boolean>(false);
  const [err, setErr] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<number | null>(null);

  useEffect(() => {
    if (!open) return;
    setErr(null);
    fetchConfig()
      .then((c) => {
        setCurrentModel(c.model);
        setCurrentHasKey(c.has_key);
        setPendingModel(c.model);
      })
      .catch((e) => setErr(String(e)));
  }, [open]);

  const isOpenAi = pendingModel.startsWith("openai:");
  const isOpenRouter = pendingModel.startsWith("openrouter:");
  const requiresKeyInput = isOpenAi && !currentHasKey;
  const canSave =
    !saving &&
    pendingModel !== "" &&
    (!requiresKeyInput || pendingKey.length > 0);

  async function onSave() {
    setSaving(true);
    setErr(null);
    try {
      const body: {
        model: string;
        openai_api_key?: string;
        openrouter_api_key?: string;
        ollama_url?: string;
      } = { model: pendingModel };
      if (isOpenAi && pendingKey) body.openai_api_key = pendingKey;
      if (isOpenRouter && pendingKey) body.openrouter_api_key = pendingKey;
      if (pendingOllama) body.ollama_url = pendingOllama;
      const r = await setConfig(body);
      setCurrentModel(r.model);
      setCurrentHasKey(r.has_key);
      setSavedAt(Date.now());
      setPendingKey("");
    } catch (e) {
      setErr(String(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <Dialog
      open={open}
      onClose={onClose}
      aria-label="settings"
      maxWidth="xs"
      fullWidth
      slotProps={{
        paper: {
          "data-testid": "settings-modal",
          sx: { fontFamily: "var(--font-space-mono), monospace" },
        } as never,
      }}
    >
      <Box sx={{ display: "flex", alignItems: "center", justifyContent: "space-between", px: 3, pt: 2.5, pb: 1 }}>
        <Typography
          component="h3"
          sx={{ ...LABEL_SX, color: "var(--info)", fontWeight: 700, fontSize: 13 }}
        >
          settings
        </Typography>
        <IconButton aria-label="close settings" onClick={onClose} size="small">
          <CloseIcon fontSize="small" />
        </IconButton>
      </Box>

      <DialogContent sx={{ pt: 1 }}>
        <Stack spacing={2}>
          <Box>
            <Typography sx={LABEL_SX}>current</Typography>
            <Box sx={{ display: "flex", justifyContent: "space-between", mt: 0.5 }}>
              <Typography sx={{ fontFamily: "var(--font-space-mono), monospace", fontSize: 12 }}>
                model
              </Typography>
              <Typography sx={{ fontFamily: "var(--font-space-mono), monospace", fontSize: 12, color: "var(--info)" }}>
                {currentModel}
              </Typography>
            </Box>
            <Box sx={{ display: "flex", justifyContent: "space-between" }}>
              <Typography sx={{ fontFamily: "var(--font-space-mono), monospace", fontSize: 12 }}>
                openai key bound
              </Typography>
              <Typography
                sx={{
                  fontFamily: "var(--font-space-mono), monospace",
                  fontSize: 12,
                  color: currentHasKey ? "var(--success)" : "var(--text-secondary)",
                }}
              >
                {currentHasKey ? "yes" : "no"}
              </Typography>
            </Box>
          </Box>

          <Divider />

          <Box>
            <Typography sx={LABEL_SX}>naming + summary model</Typography>
            <Select
              fullWidth
              size="small"
              value={pendingModel}
              onChange={(e) => setPendingModel(e.target.value as string)}
              sx={{ mt: 0.5 }}
            >
              {MODEL_PRESETS.map((p) => (
                <MenuItem key={p.value} value={p.value} sx={{ fontFamily: "var(--font-space-mono), monospace", fontSize: 12 }}>
                  {p.label}
                </MenuItem>
              ))}
            </Select>
            <Typography sx={HINT_SX}>
              Used for session names, card summaries, and "what it's waiting on" text.
            </Typography>
          </Box>

          {isOpenAi ? (
            <Box>
              <Typography sx={LABEL_SX}>
                openai api key {currentHasKey ? "(leave blank to keep current)" : ""}
              </Typography>
              <TextField
                fullWidth
                size="small"
                type="password"
                value={pendingKey}
                onChange={(e) => setPendingKey(e.target.value)}
                placeholder="sk-..."
                sx={{ mt: 0.5 }}
                slotProps={{ htmlInput: { style: { fontFamily: "var(--font-space-mono), monospace", fontSize: 12 } } }}
              />
              <Typography sx={HINT_SX}>
                Stays in-memory on the API server. Never persisted to disk.
              </Typography>
            </Box>
          ) : null}

          {isOpenRouter ? (
            <Box>
              <Typography sx={LABEL_SX}>
                openrouter api key (optional — server will fall back to env / on-disk)
              </Typography>
              <TextField
                fullWidth
                size="small"
                type="password"
                value={pendingKey}
                onChange={(e) => setPendingKey(e.target.value)}
                placeholder="sk-or-v1-... (leave blank to use ~/.config/openrouter/api_key)"
                sx={{ mt: 0.5 }}
                slotProps={{ htmlInput: { style: { fontFamily: "var(--font-space-mono), monospace", fontSize: 12 } } }}
              />
              <Typography sx={HINT_SX}>
                Pre-filled from the operator-convention path on the API host; leave blank to use it.
              </Typography>
            </Box>
          ) : null}

          {pendingModel.startsWith("local:") ? (
            <Box>
              <Typography sx={LABEL_SX}>ollama url (optional)</Typography>
              <TextField
                fullWidth
                size="small"
                type="text"
                value={pendingOllama}
                onChange={(e) => setPendingOllama(e.target.value)}
                placeholder="http://127.0.0.1:11434"
                sx={{ mt: 0.5 }}
                slotProps={{ htmlInput: { style: { fontFamily: "var(--font-space-mono), monospace", fontSize: 12 } } }}
              />
            </Box>
          ) : null}

          {err ? (
            <Typography sx={{ ...HINT_SX, color: "var(--accent)" }}>error: {err}</Typography>
          ) : null}
          {savedAt ? (
            <Typography sx={{ ...HINT_SX, color: "var(--success)" }}>
              saved · cache cleared
            </Typography>
          ) : null}
        </Stack>
      </DialogContent>

      <DialogActions sx={{ px: 3, pb: 2.5 }}>
        <Button variant="outlined" onClick={onClose}>
          close
        </Button>
        <Button variant="contained" onClick={onSave} disabled={!canSave}>
          {saving ? "saving…" : "save"}
        </Button>
      </DialogActions>
    </Dialog>
  );
}
