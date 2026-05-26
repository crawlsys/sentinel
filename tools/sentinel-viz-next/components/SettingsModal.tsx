"use client";

import { useEffect, useState } from "react";

import { fetchConfig, setConfig } from "../lib/api";

interface Props {
  open: boolean;
  onClose: () => void;
}

const MODEL_PRESETS = [
  { value: "none", label: "Disabled (UUID fallback)" },
  // OpenRouter group — instance-wide default per P3-32. Cheap +
  // good options first, then a few stronger picks for when the
  // operator wants longer / more accurate summaries.
  { value: "openrouter:openai/gpt-4o-mini", label: "OpenRouter · gpt-4o-mini (cheap+good, default)" },
  { value: "openrouter:google/gemini-2.0-flash-001", label: "OpenRouter · gemini-2.0-flash (very cheap)" },
  { value: "openrouter:anthropic/claude-3.5-haiku", label: "OpenRouter · claude-3.5-haiku" },
  { value: "openrouter:openai/gpt-4o", label: "OpenRouter · gpt-4o (stronger, costlier)" },
  // Direct OpenAI — only relevant if operator wants to bypass
  // OpenRouter and pay OpenAI directly.
  { value: "openai:gpt-4o-mini", label: "OpenAI direct · gpt-4o-mini" },
  { value: "openai:gpt-4o", label: "OpenAI direct · gpt-4o" },
  // Local Ollama for fully-private workflows.
  { value: "local:qwen2.5:1.5b", label: "Local · qwen2.5:1.5b (fast, private)" },
  { value: "local:qwen2.5-coder:7b", label: "Local · qwen2.5-coder:7b" },
];

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

  if (!open) return null;

  const isOpenAi = pendingModel.startsWith("openai:");
  const isOpenRouter = pendingModel.startsWith("openrouter:");
  // openrouter has a server-side fallback (env or on-disk file),
  // so the key field is OPTIONAL for openrouter even on first save.
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
    <div
      role="dialog"
      aria-label="settings"
      data-testid="settings-modal"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="bg-[#111] border border-[#222] rounded-lg w-[480px] max-w-[95vw] p-4 font-mono text-xs">
        <div className="flex items-baseline justify-between mb-3">
          <h3 className="text-[#5B9BF6] text-sm font-bold">settings</h3>
          <button
            type="button"
            aria-label="close settings"
            onClick={onClose}
            className="text-[#999] hover:text-[#E8E8E8]"
          >
            ✕
          </button>
        </div>

        <div className="mb-3">
          <div className="text-[10px] uppercase tracking-wider text-[#999] mb-1">
            current
          </div>
          <div className="flex justify-between">
            <span>model</span>
            <span className="text-[#5B9BF6]">{currentModel}</span>
          </div>
          <div className="flex justify-between">
            <span>openai key bound</span>
            <span className={currentHasKey ? "text-[#4A9E5C]" : "text-[#999]"}>
              {currentHasKey ? "yes" : "no"}
            </span>
          </div>
        </div>

        <div className="mb-3 border-t border-[#222] pt-3">
          <label className="block text-[10px] uppercase tracking-wider text-[#999] mb-1">
            naming + summary model
          </label>
          <select
            value={pendingModel}
            onChange={(e) => setPendingModel(e.target.value)}
            className="w-full bg-[#000] border border-[#222] rounded px-2 py-1 text-[#E8E8E8]"
          >
            {MODEL_PRESETS.map((p) => (
              <option key={p.value} value={p.value}>{p.label}</option>
            ))}
          </select>
          <div className="text-[10px] text-[#999] mt-1">
            Used for session names, card summaries, and "what it's waiting on" text.
          </div>
        </div>

        {isOpenAi ? (
          <div className="mb-3">
            <label className="block text-[10px] uppercase tracking-wider text-[#999] mb-1">
              openai api key {currentHasKey ? "(leave blank to keep current)" : ""}
            </label>
            <input
              type="password"
              value={pendingKey}
              onChange={(e) => setPendingKey(e.target.value)}
              placeholder="sk-..."
              className="w-full bg-[#000] border border-[#222] rounded px-2 py-1 text-[#E8E8E8] font-mono"
            />
            <div className="text-[10px] text-[#999] mt-1">
              Stays in-memory on the API server. Never persisted to disk.
            </div>
          </div>
        ) : null}
        {isOpenRouter ? (
          <div className="mb-3">
            <label className="block text-[10px] uppercase tracking-wider text-[#999] mb-1">
              openrouter api key (optional — server will fall back to env / on-disk)
            </label>
            <input
              type="password"
              value={pendingKey}
              onChange={(e) => setPendingKey(e.target.value)}
              placeholder="sk-or-v1-... (leave blank to use ~/.config/openrouter/api_key)"
              className="w-full bg-[#000] border border-[#222] rounded px-2 py-1 text-[#E8E8E8] font-mono"
            />
            <div className="text-[10px] text-[#999] mt-1">
              Pre-filled from the operator-convention path on the API host; leave blank to use it.
            </div>
          </div>
        ) : null}

        {pendingModel.startsWith("local:") ? (
          <div className="mb-3">
            <label className="block text-[10px] uppercase tracking-wider text-[#999] mb-1">
              ollama url (optional)
            </label>
            <input
              type="text"
              value={pendingOllama}
              onChange={(e) => setPendingOllama(e.target.value)}
              placeholder="http://127.0.0.1:11434"
              className="w-full bg-[#000] border border-[#222] rounded px-2 py-1 text-[#E8E8E8] font-mono"
            />
          </div>
        ) : null}

        {err ? <div className="text-[#D71921] mb-2">error: {err}</div> : null}
        {savedAt ? (
          <div className="text-[#4A9E5C] mb-2">saved · cache cleared</div>
        ) : null}

        <div className="flex justify-end gap-2 mt-3">
          <button
            type="button"
            onClick={onClose}
            className="px-3 py-1 rounded border border-[#222] text-[#E8E8E8] hover:bg-[#222]"
          >
            close
          </button>
          <button
            type="button"
            onClick={onSave}
            disabled={!canSave}
            className="px-3 py-1 rounded bg-[#5B9BF6] text-white disabled:bg-[#222] disabled:text-[#999]"
          >
            {saving ? "saving…" : "save"}
          </button>
        </div>
      </div>
    </div>
  );
}
