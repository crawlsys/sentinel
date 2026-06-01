"use client";

import { useEffect, useRef, useState } from "react";

import { fetchConfig, setConfig } from "../lib/api";

interface Props {
  open: boolean;
  onClose: () => void;
}

const MODEL_PRESETS = [
  { value: "none", label: "Disabled (UUID fallback)" },
  { value: "openai:gpt-4o-mini", label: "OpenAI · gpt-4o-mini" },
  { value: "openai:gpt-4o", label: "OpenAI · gpt-4o" },
  { value: "local:qwen2.5:1.5b", label: "Local · qwen2.5:1.5b (fast)" },
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

  // Dialog focus management.
  const dialogRef = useRef<HTMLDivElement | null>(null);
  const firstFieldRef = useRef<HTMLSelectElement | null>(null);
  // The element focused before the modal opened, restored on close.
  const triggerRef = useRef<Element | null>(null);

  // Load config when the modal opens. setState only ever runs inside
  // the async fetch callbacks (never synchronously in the effect body),
  // and an AbortController prevents writes after unmount/close.
  useEffect(() => {
    if (!open) return;
    const abort = new AbortController();
    fetchConfig(abort.signal)
      .then((c) => {
        if (abort.signal.aborted) return;
        setCurrentModel(c.model);
        setCurrentHasKey(c.has_key);
        setPendingModel(c.model);
        setErr(null);
      })
      .catch((e) => {
        if (abort.signal.aborted) return;
        const isAbort = e instanceof Error && e.name === "AbortError";
        if (!isAbort) setErr(String(e));
      });
    return () => abort.abort();
  }, [open]);

  // Focus management: remember the trigger, focus the first field on
  // open, restore focus to the trigger on close.
  useEffect(() => {
    if (!open) return;
    triggerRef.current = document.activeElement;
    firstFieldRef.current?.focus();
    const trigger = triggerRef.current;
    return () => {
      if (trigger instanceof HTMLElement) trigger.focus();
    };
  }, [open]);

  if (!open) return null;

  // Trap Tab within the dialog and close on Escape.
  function onKeyDown(e: React.KeyboardEvent<HTMLDivElement>) {
    if (e.key === "Escape") {
      e.stopPropagation();
      onClose();
      return;
    }
    if (e.key !== "Tab") return;
    const root = dialogRef.current;
    if (!root) return;
    const focusable = root.querySelectorAll<HTMLElement>(
      'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
    );
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    const active = document.activeElement;
    if (e.shiftKey && active === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && active === last) {
      e.preventDefault();
      first.focus();
    }
  }

  const needsKey = pendingModel.startsWith("openai:");
  const canSave = !saving && pendingModel !== "" && (!needsKey || pendingKey.length > 0 || (currentHasKey && pendingModel === currentModel));

  async function onSave() {
    setSaving(true);
    setErr(null);
    try {
      const body: { model: string; openai_api_key?: string; ollama_url?: string } = {
        model: pendingModel,
      };
      if (needsKey && pendingKey) body.openai_api_key = pendingKey;
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
      ref={dialogRef}
      role="dialog"
      aria-modal="true"
      aria-label="settings"
      data-testid="settings-modal"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
      onKeyDown={onKeyDown}
    >
      <div className="bg-[#161b22] border border-[#30363d] rounded-lg w-[480px] max-w-[95vw] p-4 font-mono text-xs">
        <div className="flex items-baseline justify-between mb-3">
          <h3 className="text-[#58a6ff] text-sm font-bold">settings</h3>
          <button
            type="button"
            aria-label="close settings"
            onClick={onClose}
            className="text-[#6e7681] hover:text-[#c9d1d9]"
          >
            ✕
          </button>
        </div>

        <div className="mb-3">
          <div className="text-[10px] uppercase tracking-wider text-[#6e7681] mb-1">
            current
          </div>
          <div className="flex justify-between">
            <span>model</span>
            <span className="text-[#58a6ff]">{currentModel}</span>
          </div>
          <div className="flex justify-between">
            <span>openai key bound</span>
            <span className={currentHasKey ? "text-[#3fb950]" : "text-[#6e7681]"}>
              {currentHasKey ? "yes" : "no"}
            </span>
          </div>
        </div>

        <div className="mb-3 border-t border-[#30363d] pt-3">
          <label className="block text-[10px] uppercase tracking-wider text-[#6e7681] mb-1">
            naming + summary model
          </label>
          <select
            ref={firstFieldRef}
            value={pendingModel}
            onChange={(e) => setPendingModel(e.target.value)}
            className="w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-[#c9d1d9]"
          >
            {MODEL_PRESETS.map((p) => (
              <option key={p.value} value={p.value}>{p.label}</option>
            ))}
          </select>
          <div className="text-[10px] text-[#6e7681] mt-1">
            Used for session names, card summaries, and {"\"what it's waiting on\""} text.
          </div>
        </div>

        {needsKey ? (
          <div className="mb-3">
            <label className="block text-[10px] uppercase tracking-wider text-[#6e7681] mb-1">
              openai api key {currentHasKey ? "(leave blank to keep current)" : ""}
            </label>
            <input
              type="password"
              value={pendingKey}
              onChange={(e) => setPendingKey(e.target.value)}
              placeholder="sk-..."
              className="w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-[#c9d1d9] font-mono"
            />
            <div className="text-[10px] text-[#6e7681] mt-1">
              Stays in-memory on the API server. Never persisted to disk.
            </div>
          </div>
        ) : null}

        {pendingModel.startsWith("local:") ? (
          <div className="mb-3">
            <label className="block text-[10px] uppercase tracking-wider text-[#6e7681] mb-1">
              ollama url (optional)
            </label>
            <input
              type="text"
              value={pendingOllama}
              onChange={(e) => setPendingOllama(e.target.value)}
              placeholder="http://127.0.0.1:11434"
              className="w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-[#c9d1d9] font-mono"
            />
          </div>
        ) : null}

        {err ? <div className="text-[#f85149] mb-2">error: {err}</div> : null}
        {savedAt ? (
          <div className="text-[#3fb950] mb-2">saved · cache cleared</div>
        ) : null}

        <div className="flex justify-end gap-2 mt-3">
          <button
            type="button"
            onClick={onClose}
            className="px-3 py-1 rounded border border-[#30363d] text-[#c9d1d9] hover:bg-[#21262d]"
          >
            close
          </button>
          <button
            type="button"
            onClick={onSave}
            disabled={!canSave}
            className="px-3 py-1 rounded bg-[#1f6feb] text-white disabled:bg-[#21262d] disabled:text-[#6e7681]"
          >
            {saving ? "saving…" : "save"}
          </button>
        </div>
      </div>
    </div>
  );
}
