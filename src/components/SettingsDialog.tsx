import { createSignal, For, Show, onMount, onCleanup } from "solid-js";
import { open } from "@tauri-apps/plugin-dialog";

import {
  api,
  events,
  formatBytes,
  formatDuration,
  CommandError,
  type DownloadProgress,
  type ModelView,
  type Role,
  type SettingsView,
} from "../lib/api";

interface Props {
  settings: SettingsView;
  models: ModelView[];
  loadedModel: string | null;
  onClose: () => void;
  onChanged: () => Promise<void>;
}

const ROLE_LABEL: Record<Role, string> = {
  coding: "Programování",
  conversational: "Konverzace a čeština",
};

const CONTEXT_CHOICES = [4096, 8192, 16384, 32768, 65536, 131072];

export function SettingsDialog(props: Props) {
  const [tab, setTab] = createSignal<"modely" | "token" | "beh">("modely");
  const [token, setToken] = createSignal("");
  const [zprava, setZprava] = createSignal<{ text: string; ok: boolean } | null>(null);
  const [pracuje, setPracuje] = createSignal<string | null>(null);
  const [stahovani, setStahovani] = createSignal<DownloadProgress | null>(null);

  let unlisten: (() => void) | undefined;

  onMount(async () => {
    unlisten = await events.onDownloadProgress((p) => setStahovani(p));
  });
  onCleanup(() => unlisten?.());

  const hlaska = (text: string, ok = true) => setZprava({ text, ok });

  const chyba = (e: unknown) => {
    if (e instanceof CommandError && e.cancelled) {
      hlaska("Zrušeno.", true);
      return;
    }
    hlaska(e instanceof Error ? e.message : String(e), false);
  };

  const ulozitToken = async () => {
    setPracuje("token");
    try {
      const jmeno = await api.saveHfToken(token());
      setToken("");
      hlaska(`Token ověřen — přihlášen jako ${jmeno}.`);
      await props.onChanged();
    } catch (e) {
      chyba(e);
    } finally {
      setPracuje(null);
    }
  };

  const smazatToken = async () => {
    try {
      await api.clearHfToken();
      hlaska("Token smazán.");
      await props.onChanged();
    } catch (e) {
      chyba(e);
    }
  };

  const vybratSlozkuModelu = async () => {
    const vybrano = await open({
      directory: true,
      multiple: false,
      title: "Kam ukládat modely",
      defaultPath: props.settings.modelsDirectory ?? props.settings.defaultModelsDirectory,
    });
    if (typeof vybrano !== "string") return;
    try {
      await api.saveSettings({ models_directory: vybrano });
      hlaska("Složka pro modely uložena.");
      await props.onChanged();
    } catch (e) {
      chyba(e);
    }
  };

  const stahnout = async (m: ModelView) => {
    setPracuje(m.id);
    setStahovani(null);
    try {
      await api.ensureModel(m.id);
      hlaska(`${m.name} je připravený.`);
      await props.onChanged();
    } catch (e) {
      chyba(e);
    } finally {
      setPracuje(null);
      setStahovani(null);
    }
  };

  const pouzit = async (m: ModelView) => {
    setPracuje(m.id);
    try {
      await api.saveSettings(
        m.role === "coding" ? { coding_model: m.id } : { conversational_model: m.id },
      );
      await api.loadModel(m.id);
      hlaska(`${m.name} je načtený.`);
      await props.onChanged();
    } catch (e) {
      chyba(e);
    } finally {
      setPracuje(null);
    }
  };

  const zmenitKontext = async (tokens: number) => {
    try {
      await api.saveSettings({ context_tokens: tokens });
      hlaska("Kontext uložen. Projeví se po novém načtení modelu.");
      await props.onChanged();
    } catch (e) {
      chyba(e);
    }
  };

  const prepnoutGpu = async (use: boolean) => {
    try {
      await api.saveSettings({ use_gpu: use });
      hlaska("Uloženo. Projeví se po novém načtení modelu.");
      await props.onChanged();
    } catch (e) {
      chyba(e);
    }
  };

  return (
    <div class="overlay" onClick={(e) => e.target === e.currentTarget && props.onClose()}>
      <div class="dialog">
        <header class="dialog-head">
          <h2>Nastavení</h2>
          <button class="ghost" onClick={props.onClose}>
            ✕
          </button>
        </header>

        <nav class="tabs">
          <button classList={{ active: tab() === "modely" }} onClick={() => setTab("modely")}>
            Modely
          </button>
          <button classList={{ active: tab() === "token" }} onClick={() => setTab("token")}>
            Token HuggingFace
          </button>
          <button classList={{ active: tab() === "beh" }} onClick={() => setTab("beh")}>
            Běh modelu
          </button>
        </nav>

        <div class="dialog-body">
          <Show when={tab() === "modely"}>
            <div class="field">
              <label>Složka pro modely</label>
              <div class="row">
                <input
                  readOnly
                  value={props.settings.modelsDirectory ?? props.settings.defaultModelsDirectory}
                />
                <button onClick={vybratSlozkuModelu}>Změnit</button>
              </div>
              <p class="hint">
                Modely mají 13–19 GB. Anvil je hledá i ve složkách jiných nástrojů, takže
                co už na disku máš, se znovu nestahuje.
              </p>
            </div>

            <For each={["coding", "conversational"] as Role[]}>
              {(role) => (
                <section class="model-group">
                  <h3>{ROLE_LABEL[role]}</h3>
                  <For each={props.models.filter((m) => m.role === role)}>
                    {(m) => (
                      <article class="model-card" classList={{ active: props.loadedModel === m.id }}>
                        <div class="model-head">
                          <span class="model-name">{m.name}</span>
                          <Show when={m.recommended}>
                            <span class="badge">doporučeno</span>
                          </Show>
                          <Show when={props.loadedModel === m.id}>
                            <span class="badge badge-hot">načtený</span>
                          </Show>
                        </div>
                        <p class="model-desc">{m.description}</p>
                        <div class="model-facts">
                          <span>{formatBytes(m.sizeBytes)}</span>
                          <span>
                            {m.activeParamsB} B aktivních z {m.totalParamsB} B
                          </span>
                          <span>kontext {(m.nativeContextTokens / 1024).toFixed(0)}K</span>
                          <Show when={m.gated}>
                            <span class="warn-text">vyžaduje token</span>
                          </Show>
                        </div>

                        <Show when={pracuje() === m.id && stahovani()}>
                          {(p) => (
                            <div class="progress">
                              <div class="progress-track">
                                <div
                                  class="progress-fill"
                                  style={{
                                    width: `${
                                      p().totalBytes > 0
                                        ? (p().downloadedBytes / p().totalBytes) * 100
                                        : 0
                                    }%`,
                                  }}
                                />
                              </div>
                              <span class="progress-label">
                                {formatBytes(p().downloadedBytes)} / {formatBytes(p().totalBytes)}
                                {" · "}
                                {(p().bytesPerSecond / 1024 ** 2).toFixed(1)} MB/s
                                {" · zbývá "}
                                {formatDuration(
                                  p().bytesPerSecond > 0
                                    ? (p().totalBytes - p().downloadedBytes) / p().bytesPerSecond
                                    : 0,
                                )}
                              </span>
                            </div>
                          )}
                        </Show>

                        <div class="model-actions">
                          <Show
                            when={m.installed}
                            fallback={
                              <button
                                class="primary"
                                disabled={pracuje() !== null}
                                onClick={() => stahnout(m)}
                              >
                                {pracuje() === m.id ? "Stahuji…" : "Stáhnout"}
                              </button>
                            }
                          >
                            <button
                              class="primary"
                              disabled={pracuje() !== null || props.loadedModel === m.id}
                              onClick={() => pouzit(m)}
                            >
                              {pracuje() === m.id
                                ? "Načítám…"
                                : props.loadedModel === m.id
                                  ? "Načtený"
                                  : "Načíst"}
                            </button>
                          </Show>
                        </div>
                      </article>
                    )}
                  </For>
                </section>
              )}
            </For>
          </Show>

          <Show when={tab() === "token"}>
            <div class="field">
              <label>Token HuggingFace</label>
              <Show
                when={!props.settings.hasHfToken}
                fallback={
                  <div class="row">
                    <input readOnly value="Token je uložený v systémovém úložišti." />
                    <button onClick={smazatToken}>Smazat</button>
                  </div>
                }
              >
                <div class="row">
                  <input
                    type="password"
                    placeholder="hf_…"
                    value={token()}
                    onInput={(e) => setToken(e.currentTarget.value)}
                  />
                  <button
                    class="primary"
                    disabled={!token().trim() || pracuje() === "token"}
                    onClick={ulozitToken}
                  >
                    {pracuje() === "token" ? "Ověřuji…" : "Ověřit a uložit"}
                  </button>
                </div>
              </Show>
              <p class="hint">
                Potřebuješ ho jen pro modely, které vyžadují souhlas s licencí. Doporučené
                modely v katalogu se stáhnou i bez něj. Token se ukládá do Credential Manageru
                (Windows) nebo Keychainu (macOS) a až po ověření — překlep se pozná hned,
                ne po hodině stahování.
              </p>
            </div>
          </Show>

          <Show when={tab() === "beh"}>
            <div class="field">
              <label>Kontextové okno</label>
              <select
                value={props.settings.contextTokens}
                onChange={(e) => zmenitKontext(Number(e.currentTarget.value))}
              >
                <For each={CONTEXT_CHOICES}>
                  {(v) => <option value={v}>{(v / 1024).toFixed(0)}K tokenů</option>}
                </For>
              </select>
              <p class="hint">
                Větší okno pojme víc kódu, ale KV cache zabere víc paměti grafické karty.
                Anvil si i tak stará část konverzace průběžně slučuje, takže délka
                rozhovoru limit nenaráží.
              </p>
            </div>

            <div class="field">
              <label>
                <input
                  type="checkbox"
                  class="checkbox"
                  checked={props.settings.useGpu}
                  onChange={(e) => prepnoutGpu(e.currentTarget.checked)}
                />
                Používat grafickou kartu
              </label>
              <p class="hint">
                Rozložení modelu mezi kartu a RAM si Anvil spočítá sám z velikosti modelu
                a volné paměti. Vypnutí má smysl jen na ladění.
              </p>
            </div>
          </Show>
        </div>

        <Show when={zprava()}>
          {(z) => (
            <footer class="dialog-msg" classList={{ err: !z().ok }}>
              {z().text}
            </footer>
          )}
        </Show>
      </div>
    </div>
  );
}
