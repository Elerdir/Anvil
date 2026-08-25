/**
 * Typované obálky nad Tauri příkazy.
 *
 * Jediné místo v UI, které volá `invoke`. Díky tomu je z jednoho souboru
 * vidět celá plocha mezi frontendem a Rustem, a když se v Rustu něco
 * přejmenuje, překladač to najde tady, ne až za běhu.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// --- Tvary, které chodí z Rustu -------------------------------------------

export type Role = "coding" | "conversational";
export type MessageRole = "user" | "assistant" | "tool" | "system";

export interface ModelView {
  id: string;
  name: string;
  description: string;
  role: Role;
  sizeBytes: number;
  recommended: boolean;
  installed: boolean;
  gated: boolean;
  activeParamsB: number;
  totalParamsB: number;
  nativeContextTokens: number;
}

export interface MessageView {
  id: string;
  role: MessageRole;
  content: string;
  tokenCount: number | null;
}

export interface ConversationSummaryView {
  id: string;
  title: string;
  pinned: boolean;
  messageCount: number;
  updatedAt: string;
  /** Konverzace, ze které tahle vznikla — u odvětvených vláken. */
  parentId: string | null;
}

export interface SessionView {
  conversationId: string | null;
  generating: boolean;
  loadedModel: string | null;
  planDescription: string | null;
  workspacePath: string | null;
  workspaceName: string | null;
  conversationTitle: string;
  parentId: string | null;
  messages: MessageView[];
  usedTokens: number;
  contextTokens: number;
  hasSummary: boolean;
  engineAvailable: boolean;
}

export interface SettingsView {
  modelsDirectory: string | null;
  defaultModelsDirectory: string;
  codingModel: string | null;
  conversationalModel: string | null;
  activeRole: Role;
  contextTokens: number;
  useGpu: boolean;
  setupCompleted: boolean;
  hasHfToken: boolean;
  lastWorkspace: string | null;
}

export interface SettingsPatch {
  models_directory?: string;
  coding_model?: string;
  conversational_model?: string;
  active_role?: Role;
  context_tokens?: number;
  use_gpu?: boolean;
  setup_completed?: boolean;
}

export interface GenerationProgress {
  delta: string;
  accumulated: string;
  tokenCount: number;
}

export interface GenerationStats {
  promptTokens: number;
  generatedTokens: number;
  timeToFirstTokenMs: number;
  totalMs: number;
  tokensPerSecond: number;
  cancelled: boolean;
  compactedMessages: number | null;
}

export interface DownloadProgress {
  downloadedBytes: number;
  totalBytes: number;
  bytesPerSecond: number;
}

export type Severity = "critical" | "warning" | "note";

export interface FindingView {
  file: string;
  line: number | null;
  severity: Severity;
  summary: string;
  detail: string;
  location: string;
}

export interface ReviewReportView {
  headline: string;
  findings: FindingView[];
  filesRead: string[];
  rounds: number;
  /** Kolik souborů projekt má — proti `filesRead`, ať je vidět pokrytí. */
  filesTotal: number;
  /** Skončilo se na limitu kol, ne proto, že model dokončil práci. */
  hitRoundLimit: boolean;
  summary: string;
  totalMs: number;
}

/** Co model právě dělá. Bez toho uživatel kouká minuty na prázdné okno. */
export type AgentEventView =
  | { kind: "round"; round: number }
  | { kind: "tool_called"; name: string; summary: string }
  | { kind: "tool_finished"; name: string; ok: boolean }
  | { kind: "prose"; text: string }
  | { kind: "step"; done: number; total: number; label: string };

/** Chyba z příkazu. `cancelled` znamená, že to zrušil uživatel. */
export class CommandError extends Error {
  readonly cancelled: boolean;

  constructor(message: string, cancelled = false) {
    super(message);
    this.name = "CommandError";
    this.cancelled = cancelled;
  }
}

// --- Volání ---------------------------------------------------------------

/**
 * Rust posílá pole ve `snake_case`, UI je chce v `camelCase`. Převod je tady
 * na jednom místě, aby se v komponentách nemíchaly dvě konvence.
 */
function toCamel<T>(value: unknown): T {
  if (Array.isArray(value)) {
    return value.map((v) => toCamel(v)) as T;
  }
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value)) {
      const key = k.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase());
      out[key] = toCamel(v);
    }
    return out as T;
  }
  return value as T;
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return toCamel<T>(await invoke(command, args));
  } catch (raw) {
    // Rust vrací { message, cancelled }; cokoli jiného je chyba samotného
    // mostu (např. běh v prohlížeči bez Tauri).
    if (raw && typeof raw === "object" && "message" in raw) {
      const e = raw as { message: string; cancelled?: boolean };
      throw new CommandError(e.message, e.cancelled ?? false);
    }
    throw new CommandError(String(raw));
  }
}

export const api = {
  getSettings: () => call<SettingsView>("get_settings"),
  saveSettings: (patch: SettingsPatch) => call<SettingsView>("save_settings", { patch }),

  saveHfToken: (token: string) => call<string>("save_hf_token", { token }),
  clearHfToken: () => call<void>("clear_hf_token"),

  listModels: () => call<ModelView[]>("list_models"),
  ensureModel: (modelId: string) => call<string>("ensure_model", { modelId }),
  loadModel: (modelId: string) => call<SessionView>("load_model", { modelId }),
  unloadModel: () => call<SessionView>("unload_model"),

  setWorkspace: (path: string | null) => call<SessionView>("set_workspace", { path }),

  getSession: () => call<SessionView>("get_session"),
  newConversation: () => call<SessionView>("new_conversation"),

  listConversations: () => call<ConversationSummaryView[]>("list_conversations"),
  openConversation: (id: string) => call<SessionView>("open_conversation", { id }),
  renameConversation: (id: string, title: string) =>
    call<void>("rename_conversation", { id, title }),
  pinConversation: (id: string, pinned: boolean) =>
    call<void>("pin_conversation", { id, pinned }),
  reorderConversations: (ids: string[]) => call<void>("reorder_conversations", { ids }),
  deleteConversation: (id: string) => call<SessionView>("delete_conversation", { id }),
  /** Odvětví nové vlákno včetně zvolené zprávy — „odsud jinudy“. */
  branchConversation: (messageId: string) =>
    call<SessionView>("branch_conversation", { messageId }),
  /** Odvětví nové vlákno před zvolenou zprávou — „zeptat se znovu jinak“. */
  branchBeforeMessage: (messageId: string) =>
    call<SessionView>("branch_before_message", { messageId }),

  sendMessage: (text: string) => call<SessionView>("send_message", { text }),
  cancelGeneration: () => call<boolean>("cancel_generation"),

  runReview: (focus: string | null) => call<ReviewReportView>("run_review", { focus }),
  listTools: () => call<string[]>("list_tools"),
};

// --- Události -------------------------------------------------------------

/**
 * Události chodí ze stejného Rustu jako příkazy, tedy taky ve `snake_case`.
 * Prochází proto stejným převodem — jedna konvence v celém UI.
 */
function on<T>(name: string, fn: (payload: T) => void): Promise<UnlistenFn> {
  return listen<unknown>(name, (e) => fn(toCamel<T>(e.payload)));
}

export const events = {
  onGenerationDelta: (fn: (p: GenerationProgress) => void) =>
    on<GenerationProgress>("generation:delta", fn),

  onGenerationFinished: (fn: (s: GenerationStats) => void) =>
    on<GenerationStats>("generation:finished", fn),

  onDownloadProgress: (fn: (p: DownloadProgress) => void) =>
    on<DownloadProgress>("download:progress", fn),

  onAgentEvent: (fn: (e: AgentEventView) => void) => on<AgentEventView>("agent:event", fn),
};

// --- Formátování ----------------------------------------------------------

export function formatBytes(bytes: number): string {
  if (bytes <= 0) return "—";
  const gb = bytes / 1024 ** 3;
  if (gb >= 1) return `${gb.toFixed(1)} GB`;
  return `${(bytes / 1024 ** 2).toFixed(0)} MB`;
}

export function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "—";
  if (seconds < 60) return `${Math.round(seconds)} s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m} min`;
  return `${Math.floor(m / 60)} h ${m % 60} min`;
}
