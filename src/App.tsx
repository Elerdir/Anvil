import { createSignal, onCleanup, onMount, Show } from "solid-js";

import { Composer } from "./components/Composer";
import { MessageList } from "./components/MessageList";
import { SettingsDialog } from "./components/SettingsDialog";
import {
  api,
  CommandError,
  events,
  type GenerationStats,
  type ModelView,
  type Role,
  type SessionView,
  type SettingsView,
} from "./lib/api";

const ROLE_LABEL: Record<Role, string> = {
  coding: "Kód",
  conversational: "Čeština",
};

export function App() {
  const [session, setSession] = createSignal<SessionView | null>(null);
  const [settings, setSettings] = createSignal<SettingsView | null>(null);
  const [models, setModels] = createSignal<ModelView[]>([]);
  const [streaming, setStreaming] = createSignal("");
  const [generating, setGenerating] = createSignal(false);
  const [stats, setStats] = createSignal<GenerationStats | null>(null);
  const [chyba, setChyba] = createSignal<string | null>(null);
  const [nastaveniOtevrena, setNastaveniOtevrena] = createSignal(false);

  const unlisteners: Array<() => void> = [];

  const nacistVse = async () => {
    try {
      const [s, n, m] = await Promise.all([
        api.getSession(),
        api.getSettings(),
        api.listModels(),
      ]);
      setSession(s);
      setSettings(n);
      setModels(m);
      setChyba(null);
    } catch (e) {
      setChyba(e instanceof Error ? e.message : String(e));
    }
  };

  onMount(async () => {
    await nacistVse();

    try {
      unlisteners.push(
        await events.onGenerationDelta((p) => setStreaming(p.accumulated)),
        await events.onGenerationFinished((s) => setStats(s)),
      );
    } catch (e) {
      // Bez odběru událostí appka pořád funguje, jen odpověď naskočí naráz
      // místo po tokenech. Neodchycená výjimka by naopak utnula zbytek
      // `onMount` a nikde by se neprojevila.
      console.error("Odběr událostí se nepodařilo navázat:", e);
    }

    // Model se nenačítá sám: 16GB soubor a desítky sekund při startu by
    // z appky udělaly něco, co se „nespouští". Uživatel to spustí z nastavení.
  });

  onCleanup(() => unlisteners.forEach((u) => u()));

  const odeslat = async (text: string) => {
    setGenerating(true);
    setStreaming("");
    setStats(null);
    setChyba(null);
    try {
      setSession(await api.sendMessage(text));
    } catch (e) {
      if (e instanceof CommandError && e.cancelled) {
        // Zrušil to uživatel — částečná odpověď už v konverzaci je.
        setSession(await api.getSession());
      } else {
        setChyba(e instanceof Error ? e.message : String(e));
        setSession(await api.getSession());
      }
    } finally {
      setGenerating(false);
      setStreaming("");
    }
  };

  const zrusit = () => {
    void api.cancelGeneration();
  };

  const zmenitSlozku = async (path: string | null) => {
    try {
      setSession(await api.setWorkspace(path));
      setChyba(null);
    } catch (e) {
      setChyba(e instanceof Error ? e.message : String(e));
    }
  };

  const prepnoutRoli = async () => {
    const n = settings();
    if (!n) return;
    const nova: Role = n.activeRole === "coding" ? "conversational" : "coding";
    try {
      setSettings(await api.saveSettings({ active_role: nova }));
      // Model se nepřepíná sám — dva se do paměti nevejdou a načtení trvá.
      // Uživatel ho spustí v nastavení, až bude chtít.
    } catch (e) {
      setChyba(e instanceof Error ? e.message : String(e));
    }
  };

  const novaKonverzace = async () => {
    setStats(null);
    setSession(await api.newConversation());
  };

  const modelNazev = () => {
    const id = session()?.loadedModel;
    return models().find((m) => m.id === id)?.name ?? null;
  };

  return (
    <div class="app">
      <header class="topbar">
        <div class="brand">
          <span class="brand-mark">▣</span>
          <span class="brand-name">Anvil</span>
        </div>

        <div class="topbar-status" title={session()?.planDescription ?? ""}>
          <Show
            when={session()?.loadedModel}
            fallback={<span class="status-idle">Není načtený model</span>}
          >
            <span class="status-dot" />
            <span class="status-model">{modelNazev()}</span>
            <Show when={session()?.planDescription}>
              <span class="status-plan">{session()!.planDescription}</span>
            </Show>
          </Show>
        </div>

        <div class="topbar-actions">
          <Show when={settings()}>
            {(n) => (
              <button class="role-switch" onClick={prepnoutRoli} title="Přepnout režim">
                {ROLE_LABEL[n().activeRole]}
              </button>
            )}
          </Show>
          <button class="ghost" onClick={novaKonverzace} title="Nová konverzace">
            Nová
          </button>
          <button class="ghost" onClick={() => setNastaveniOtevrena(true)} title="Nastavení">
            Nastavení
          </button>
        </div>
      </header>

      <Show when={session() && !session()!.engineAvailable}>
        <div class="banner">
          Tenhle build je bez enginu llama.cpp — model se nenačte. Spusť appku přes{" "}
          <code>run.bat</code> (Windows) nebo <code>scripts/run-mac.sh</code> (macOS).
        </div>
      </Show>

      <Show when={chyba()}>
        {(e) => (
          <div class="banner banner-error">
            {e()}
            <button class="ghost" onClick={() => setChyba(null)}>
              ✕
            </button>
          </div>
        )}
      </Show>

      <MessageList
        messages={session()?.messages ?? []}
        streaming={streaming()}
        generating={generating()}
        hasSummary={session()?.hasSummary ?? false}
      />

      <Show when={stats()}>
        {(s) => (
          <div class="stats">
            {s().generatedTokens} tokenů · {s().tokensPerSecond.toFixed(1)} tok/s · první token{" "}
            {(s().timeToFirstTokenMs / 1000).toFixed(1)} s
            <Show when={s().compactedMessages}>
              {(n) => <> · sloučeno {n()} starších zpráv</>}
            </Show>
          </div>
        )}
      </Show>

      <Composer
        workspacePath={session()?.workspacePath ?? null}
        workspaceName={session()?.workspaceName ?? null}
        disabled={!session()?.loadedModel || generating()}
        generating={generating()}
        usedTokens={session()?.usedTokens ?? 0}
        contextTokens={session()?.contextTokens ?? 0}
        onSend={odeslat}
        onCancel={zrusit}
        onWorkspaceChange={zmenitSlozku}
      />

      <Show when={nastaveniOtevrena() && settings()}>
        {(n) => (
          <SettingsDialog
            settings={n()}
            models={models()}
            loadedModel={session()?.loadedModel ?? null}
            onClose={() => setNastaveniOtevrena(false)}
            onChanged={nacistVse}
          />
        )}
      </Show>
    </div>
  );
}
