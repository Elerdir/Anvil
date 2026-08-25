import { createSignal, For, Show } from "solid-js";

import type { PendingEditView } from "../lib/api";

interface Props {
  edits: PendingEditView[];
  /** Právě se zapisuje — tlačítka musí být zamčená, ať se nezapíše dvakrát. */
  busy: boolean;
  onApply: (paths: string[]) => void;
  onDiscard: (paths: string[] | null) => void;
}

/**
 * Návrhy úprav čekající na schválení.
 *
 * Dvě věci jsou tu záměrné a obě jsou o důvěře:
 *
 * Tlačítko **Zapsat** je jediná cesta, kudy se model dostane na disk. Do té
 * doby jsou změny jen spočítané a soubor je nedotčený.
 *
 * Diff se ukazuje **rozbalený**, ne schovaný za „zobrazit změny". Potvrzení,
 * které se dá odklikat bez čtení, nechrání před ničím — a to je celý smysl
 * téhle obrazovky.
 */
export function PendingEdits(props: Props) {
  // Odloží se jen ty, které uživatel vysloveně odškrtne. Výchozí je „zapsat“,
  // protože o návrh sám požádal; ale nic se nezapíše bez kliknutí.
  const [odmitnute, setOdmitnute] = createSignal<string[]>([]);

  const vybrane = () =>
    props.edits.map((e) => e.path).filter((p) => !odmitnute().includes(p));

  const prepnout = (path: string) =>
    setOdmitnute((v) =>
      v.includes(path) ? v.filter((p) => p !== path) : [...v, path],
    );

  const celkem = () => ({
    added: props.edits.reduce((s, e) => s + e.added, 0),
    removed: props.edits.reduce((s, e) => s + e.removed, 0),
  });

  return (
    <section class="edits">
      <header class="edits-head">
        <div>
          <span class="edits-title">
            {props.edits.length === 1
              ? "1 navržená úprava"
              : `${props.edits.length} navržených úprav`}
          </span>
          <span class="edits-meta">
            +{celkem().added}, −{celkem().removed} · na disku zatím nic
          </span>
        </div>
        <div class="edits-actions">
          <button
            class="ghost"
            disabled={props.busy}
            onClick={() => props.onDiscard(null)}
          >
            Zahodit vše
          </button>
          <button
            class="primary"
            disabled={props.busy || vybrane().length === 0}
            onClick={() => props.onApply(vybrane())}
          >
            {props.busy
              ? "Zapisuje se…"
              : `Zapsat ${vybrane().length === props.edits.length ? "vše" : `(${vybrane().length})`}`}
          </button>
        </div>
      </header>

      <For each={props.edits}>
        {(e) => (
          <article class="edit" classList={{ odmitnuty: odmitnute().includes(e.path) }}>
            <header class="edit-head">
              <label class="edit-pick">
                <input
                  type="checkbox"
                  checked={!odmitnute().includes(e.path)}
                  disabled={props.busy}
                  onChange={() => prepnout(e.path)}
                />
                <code>{e.path}</code>
              </label>
              <span class="edit-meta">
                <Show when={e.createsFile}>
                  <span class="edit-tag">nový soubor</span>
                </Show>
                <Show when={e.edits > 1}>
                  <span class="edit-tag">{e.edits} úpravy</span>
                </Show>
                <span class="edit-plus">+{e.added}</span>
                <span class="edit-minus">−{e.removed}</span>
              </span>
            </header>

            <pre class="diff">
              <For each={e.lines}>
                {(l) => (
                  <div class={`diff-line diff-${l.kind}`}>
                    <span class="diff-num">{l.kind === "added" ? "" : l.line}</span>
                    <span class="diff-mark">
                      {l.kind === "added" ? "+" : l.kind === "removed" ? "−" : " "}
                    </span>
                    <span class="diff-text">{l.text}</span>
                  </div>
                )}
              </For>
            </pre>

            {/* Bez tohohle by uživatel odsouhlasil i to, co neviděl. */}
            <Show when={e.truncated}>
              <p class="edit-warning">
                Náhled je zkrácený. Zbytek změny tu vidět není — zapiš ji jen
                tehdy, když téhle úpravě věříš i bez zbytku.
              </p>
            </Show>
          </article>
        )}
      </For>
    </section>
  );
}
