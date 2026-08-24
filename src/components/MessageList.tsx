import { createEffect, For, Show, onCleanup } from "solid-js";

import type { MessageView } from "../lib/api";

interface Props {
  messages: MessageView[];
  /** Text, který se právě streamuje. Prázdný, když se negeneruje. */
  streaming: string;
  generating: boolean;
  hasSummary: boolean;
  /** Odvětvit nové vlákno včetně téhle zprávy. */
  onBranch: (messageId: string) => void;
  /** Odvětvit nové vlákno před touhle zprávou a její text nabídnout k úpravě. */
  onAskAgain: (messageId: string, text: string) => void;
}

/**
 * Rozdělí text na běžné odstavce a bloky kódu.
 *
 * Plný markdown to není a schválně — pro odpovědi o kódu je podstatné jen to,
 * aby se blok v trojitých zpětných apostrofech zobrazil neproporcionálním
 * písmem a nerozpadl se. Zbytek by přidal závislost a rizika (HTML injection)
 * bez odpovídajícího přínosu.
 */
function segments(text: string): Array<{ kind: "text" | "code"; body: string; lang?: string }> {
  const out: Array<{ kind: "text" | "code"; body: string; lang?: string }> = [];
  const re = /```([\w+-]*)\n?([\s\S]*?)(?:```|$)/g;
  let last = 0;
  let m: RegExpExecArray | null;

  while ((m = re.exec(text)) !== null) {
    if (m.index > last) {
      out.push({ kind: "text", body: text.slice(last, m.index) });
    }
    out.push({ kind: "code", body: m[2] ?? "", lang: m[1] || undefined });
    last = re.lastIndex;
  }
  if (last < text.length) {
    out.push({ kind: "text", body: text.slice(last) });
  }
  return out.filter((s) => s.body.trim().length > 0 || s.kind === "code");
}

function Body(props: { content: string }) {
  return (
    <For each={segments(props.content)}>
      {(seg) =>
        seg.kind === "code" ? (
          <pre>
            <Show when={seg.lang}>
              <div class="code-lang">{seg.lang}</div>
            </Show>
            <code>{seg.body}</code>
          </pre>
        ) : (
          <p class="msg-text">{seg.body}</p>
        )
      }
    </For>
  );
}

export function MessageList(props: Props) {
  let scroller: HTMLDivElement | undefined;
  /** Uživatel odscrolloval nahoru — pak se mu nesmí skákat na konec. */
  let pinned = true;

  const onScroll = () => {
    if (!scroller) return;
    const zbyva = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    pinned = zbyva < Math.max(40, scroller.clientHeight * 0.1);
  };

  createEffect(() => {
    // Odběr na obojí: nová zpráva i každý kus streamu.
    props.messages.length;
    props.streaming;
    if (!scroller || !pinned) return;
    queueMicrotask(() => {
      if (scroller) scroller.scrollTop = scroller.scrollHeight;
    });
  });

  onCleanup(() => {
    scroller = undefined;
  });

  return (
    <div class="messages" ref={scroller} onScroll={onScroll}>
      <Show when={props.hasSummary}>
        <div class="summary-note">
          Starší část konverzace je sloučená do souhrnu, aby se vešla do
          kontextového okna.
        </div>
      </Show>

      <Show when={props.messages.length === 0 && !props.generating}>
        <div class="empty">
          <div class="empty-title">Zeptej se na cokoli ke svému projektu</div>
          <div class="empty-hint">
            Vyber složku pod oknem dotazu a Anvil bude vědět, o čem mluvíš.
          </div>
        </div>
      </Show>

      <For each={props.messages}>
        {(m) => (
          <div class={`msg msg-${m.role}`}>
            <div class="msg-who">{m.role === "user" ? "Ty" : "Anvil"}</div>
            <div class="msg-body">
              <Body content={m.content} />
              <Show when={m.tokenCount}>
                <div class="msg-meta">{m.tokenCount} tokenů</div>
              </Show>
            </div>

            {/* Akce se ukazují až při najetí myší: u každé zprávy visí
                natrvalo dvě tlačítka, což by z konverzace udělalo lištu. */}
            <div class="msg-actions">
              <Show
                when={m.role === "user"}
                fallback={
                  <button
                    class="msg-action"
                    disabled={props.generating}
                    title="Založit nové vlákno, které pokračuje od téhle odpovědi"
                    onClick={() => props.onBranch(m.id)}
                  >
                    ⑂ Větvit odsud
                  </button>
                }
              >
                <button
                  class="msg-action"
                  disabled={props.generating}
                  title="Založit nové vlákno a tenhle dotaz položit jinak"
                  onClick={() => props.onAskAgain(m.id, m.content)}
                >
                  ⑂ Zeptat se znovu
                </button>
              </Show>
            </div>
          </div>
        )}
      </For>

      <Show when={props.generating}>
        <div class="msg msg-assistant">
          <div class="msg-who">Anvil</div>
          <div class="msg-body">
            <Show
              when={props.streaming}
              fallback={<div class="thinking">přemýšlí…</div>}
            >
              <Body content={props.streaming} />
            </Show>
          </div>
        </div>
      </Show>
    </div>
  );
}
