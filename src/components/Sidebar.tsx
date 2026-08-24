import { createSignal, For, Show } from "solid-js";

import type { ConversationSummaryView } from "../lib/api";

interface Props {
  conversations: ConversationSummaryView[];
  activeId: string | null;
  /** Ve které konverzaci právě běží generování. */
  generatingId: string | null;
  onNew: () => void;
  onOpen: (id: string) => void;
  onRename: (id: string, title: string) => void;
  onPin: (id: string, pinned: boolean) => void;
  onReorder: (ids: string[]) => void;
  onDelete: (id: string) => void;
  onSettings: () => void;
}

/**
 * Seznam konverzací.
 *
 * Přerovnání jde přes nativní HTML5 drag & drop. Kdyby se pořadí měnilo až
 * po odpovědi ze serveru, položka by při puštění na okamžik skočila zpátky —
 * seznam se proto přerovná hned lokálně a teprve pak se uloží.
 */
export function Sidebar(props: Props) {
  const [dragged, setDragged] = createSignal<string | null>(null);
  const [dragOver, setDragOver] = createSignal<string | null>(null);
  const [editing, setEditing] = createSignal<string | null>(null);
  const [draft, setDraft] = createSignal("");

  const pusteno = (cil: string) => {
    const zdroj = dragged();
    setDragged(null);
    setDragOver(null);
    if (!zdroj || zdroj === cil) return;

    const poradi = props.conversations.map((c) => c.id);
    const z = poradi.indexOf(zdroj);
    const k = poradi.indexOf(cil);
    if (z < 0 || k < 0) return;

    poradi.splice(k, 0, ...poradi.splice(z, 1));
    props.onReorder(poradi);
  };

  const zacitPrejmenovat = (c: ConversationSummaryView) => {
    setEditing(c.id);
    setDraft(c.title);
  };

  const dokoncitPrejmenovani = () => {
    const id = editing();
    const nazev = draft().trim();
    setEditing(null);
    // Prázdný název by konverzaci v seznamu zneviditelnil.
    if (id && nazev) props.onRename(id, nazev);
  };

  return (
    <aside class="sidebar">
      <header class="sidebar-head">
        <button class="new-chat" onClick={props.onNew} title="Nová konverzace">
          <span class="plus">+</span>
          <span>Nová konverzace</span>
        </button>
      </header>

      <div class="chat-list">
        <Show when={props.conversations.length === 0}>
          <p class="chat-list-empty">Zatím tu nic není.</p>
        </Show>

        <For each={props.conversations}>
          {(c) => (
            <div
              class="chat-item"
              classList={{
                active: c.id === props.activeId,
                pinned: c.pinned,
                over: dragOver() === c.id,
                dragging: dragged() === c.id,
              }}
              draggable={editing() !== c.id}
              onDragStart={() => setDragged(c.id)}
              onDragEnd={() => {
                setDragged(null);
                setDragOver(null);
              }}
              onDragOver={(e) => {
                e.preventDefault();
                setDragOver(c.id);
              }}
              onDragLeave={() => dragOver() === c.id && setDragOver(null)}
              onDrop={(e) => {
                e.preventDefault();
                pusteno(c.id);
              }}
              onClick={() => editing() !== c.id && props.onOpen(c.id)}
              onDblClick={() => zacitPrejmenovat(c)}
            >
              <Show when={c.id === props.generatingId}>
                <span class="chat-busy" title="Právě odpovídá" />
              </Show>
              <Show when={c.pinned && c.id !== props.generatingId}>
                <span class="chat-pin-mark" title="Připnuto">
                  ▪
                </span>
              </Show>

              <Show
                when={editing() === c.id}
                fallback={
                  <span class="chat-title" title={c.title}>
                    {c.title}
                  </span>
                }
              >
                <input
                  class="chat-rename"
                  value={draft()}
                  autofocus
                  onClick={(e) => e.stopPropagation()}
                  onInput={(e) => setDraft(e.currentTarget.value)}
                  onBlur={dokoncitPrejmenovani}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") dokoncitPrejmenovani();
                    if (e.key === "Escape") setEditing(null);
                  }}
                />
              </Show>

              <div class="chat-actions">
                <button
                  class="ghost chat-action"
                  title={c.pinned ? "Odepnout" : "Připnout"}
                  onClick={(e) => {
                    e.stopPropagation();
                    props.onPin(c.id, !c.pinned);
                  }}
                >
                  {c.pinned ? "▪" : "▫"}
                </button>
                <button
                  class="ghost chat-action"
                  title="Přejmenovat"
                  onClick={(e) => {
                    e.stopPropagation();
                    zacitPrejmenovat(c);
                  }}
                >
                  ✎
                </button>
                <button
                  class="ghost chat-action danger"
                  title="Smazat"
                  onClick={(e) => {
                    e.stopPropagation();
                    props.onDelete(c.id);
                  }}
                >
                  ✕
                </button>
              </div>
            </div>
          )}
        </For>
      </div>

      <footer class="sidebar-foot">
        <button class="settings-btn" onClick={props.onSettings}>
          Nastavení
        </button>
      </footer>
    </aside>
  );
}
