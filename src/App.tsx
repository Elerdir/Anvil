import { createSignal, onCleanup, onMount, Show } from "solid-js";

import { Composer } from "./components/Composer";
import { MessageList } from "./components/MessageList";
import { SettingsDialog } from "./components/SettingsDialog";
import { Sidebar } from "./components/Sidebar";
import {
  api,
  CommandError,
  events,
  type ConversationSummaryView,
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
  const [chats, setChats] = createSignal<ConversationSummaryView[]>([]);
  const [streaming, setStreaming] = createSignal("");
  const [generating, setGenerating] = createSignal(false);
  const [stats, setStats] = createSignal<GenerationStats | null>(null);
  const [chyba, setChyba] = createSignal<string | null>(null);
  const [nastaveniOtevrena, setNastaveniOtevrena] = createSignal(false);

  const unlisteners: Array<() => void> = [];

  const hlasit = (e: unknown) => setChyba(e instanceof Error ? e.message : String(e));

  const nacistChaty = async () => {
    try {
      setChats(await api.listConversations());
    } catch (e) {
      hlasit(e);
    }
  };

  const nacistVse = async () => {
    try {
      const [s, n, m, c] = await Promise.all([
        api.getSession(),
        api.getSettings(),
        api.listModels(),
        api.listConversations(),
      ]);
      setSession(s);
      setSettings(n);
      setModels(m);
      setChats(c);
      setChyba(null);
    } catch (e) {
      hlasit(e);
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
      if (!(e instanceof CommandError && e.cancelled)) {
        hlasit(e);
      }
      // Po zrušení i po chybě je v konverzaci to, co stihlo vzniknout.
      setSession(await api.getSession());
    } finally {
      setGenerating(false);
      setStreaming("");
      // Název se odvozuje z prvního dotazu, takže se seznam musí obnovit.
      await nacistChaty();
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
      hlasit(e);
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
      hlasit(e);
    }
  };

  const novaKonverzace = async () => {
    setStats(null);
    try {
      setSession(await api.newConversation());
      await nacistChaty();
    } catch (e) {
      hlasit(e);
    }
  };

  const otevrit = async (id: string) => {
    if (generating()) return; // přepnutí uprostřed odpovědi by ji utnulo
    setStats(null);
    try {
      setSession(await api.openConversation(id));
      await nacistChaty();
    } catch (e) {
      hlasit(e);
    }
  };

  const prejmenovat = async (id: string, title: string) => {
    // Seznam se přepíše hned, ať název neposkočí zpátky a pak dopředu.
    setChats((v) => v.map((c) => (c.id === id ? { ...c, title } : c)));
    try {
      await api.renameConversation(id, title);
    } catch (e) {
      hlasit(e);
    }
    await nacistChaty();
  };

  const pripnout = async (id: string, pinned: boolean) => {
    try {
      await api.pinConversation(id, pinned);
      await nacistChaty();
    } catch (e) {
      hlasit(e);
    }
  };

  const prerovnat = async (ids: string[]) => {
    // Nejdřív lokálně, ať položka po puštění nepřeskočí zpátky.
    const podleId = new Map(chats().map((c) => [c.id, c]));
    const nove = ids.map((id) => podleId.get(id)).filter((c): c is ConversationSummaryView => !!c);
    setChats(nove);
    try {
      await api.reorderConversations(ids);
    } catch (e) {
      hlasit(e);
      await nacistChaty();
    }
  };

  const smazat = async (id: string) => {
    try {
      setSession(await api.deleteConversation(id));
      await nacistChaty();
    } catch (e) {
      hlasit(e);
    }
  };

  const modelNazev = () => {
    const id = session()?.loadedModel;
    return models().find((m) => m.id === id)?.name ?? null;
  };

  return (
    <div class="shell">
      <Sidebar
        conversations={chats()}
        activeId={session()?.conversationId ?? null}
        generatingId={generating() ? (session()?.conversationId ?? null) : null}
        onNew={novaKonverzace}
        onOpen={otevrit}
        onRename={prejmenovat}
        onPin={pripnout}
        onReorder={prerovnat}
        onDelete={smazat}
        onSettings={() => setNastaveniOtevrena(true)}
      />

      <main class="main">
        <header class="topbar">
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

          <Show when={settings()}>
            {(n) => (
              <button class="role-switch" onClick={prepnoutRoli} title="Přepnout režim">
                {ROLE_LABEL[n().activeRole]}
              </button>
            )}
          </Show>
        </header>

        {/* Kontejner je tu vždycky, i prázdný. Kdyby se podmíněné pruhy
            objevovaly a mizely jako přímí potomci mřížky, posunuly by se
            řádky a `1fr` by dostalo pole pro dotaz místo konverzace —
            přesně to dělalo, že pole viselo uprostřed okna. */}
        <div class="notices">
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
        </div>

        <MessageList
          messages={session()?.messages ?? []}
          streaming={streaming()}
          generating={generating()}
          hasSummary={session()?.hasSummary ?? false}
        />

        <div class="dock">
          <Show when={stats()}>
            {(s) => (
              <div class="stats">
                {s().generatedTokens} tokenů · {s().tokensPerSecond.toFixed(1)} tok/s · první
                token {(s().timeToFirstTokenMs / 1000).toFixed(1)} s
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
        </div>
      </main>

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
