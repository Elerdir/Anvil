import { createEffect, createSignal, Show } from "solid-js";
import { open } from "@tauri-apps/plugin-dialog";

interface Props {
  workspacePath: string | null;
  workspaceName: string | null;
  disabled: boolean;
  generating: boolean;
  usedTokens: number;
  contextTokens: number;
  onSend: (text: string) => void;
  onCancel: () => void;
  onWorkspaceChange: (path: string | null) => void;
  onReview: () => void;
  /**
   * Text k vložení do pole. Nese se jako objekt schválně: vložit se má
   * i podruhé stejné znění, a to jde poznat jen podle nové identity.
   */
  draft: { text: string } | null;
}

/**
 * Pole pro dotaz a nad ním výběr složky projektu.
 *
 * Výběr složky je záměrně tady a ne v nastavení: je to volba, která patří
 * ke konkrétnímu dotazu („zkontroluj mi tenhle projekt"), ne ke konfiguraci
 * aplikace, a mění se často.
 */
export function Composer(props: Props) {
  const [text, setText] = createSignal("");
  let area: HTMLTextAreaElement | undefined;

  const zaplneni = () =>
    props.contextTokens > 0
      ? Math.min(100, Math.round((props.usedTokens / props.contextTokens) * 100))
      : 0;

  const vybratSlozku = async () => {
    const vybrano = await open({
      directory: true,
      multiple: false,
      title: "Vyber složku projektu",
      defaultPath: props.workspacePath ?? undefined,
    });
    if (typeof vybrano === "string") {
      props.onWorkspaceChange(vybrano);
    }
  };

  const odeslat = () => {
    const t = text().trim();
    if (!t || props.disabled) return;
    props.onSend(t);
    setText("");
    if (area) area.style.height = "auto";
  };

  const onKeyDown = (e: KeyboardEvent) => {
    // Enter odesílá, Shift+Enter dělá nový řádek — jako v každém chatu.
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      odeslat();
    }
  };

  /** Pole roste s textem, ale jen do výšky, po které ještě zbude na konverzaci. */
  const autoResize = (el: HTMLTextAreaElement) => {
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 220)}px`;
  };

  createEffect(() => {
    const d = props.draft;
    if (!d) return;
    setText(d.text);
    // Až po překreslení: dřív by `scrollHeight` odpovídalo starému obsahu
    // a delší dotaz by zůstal schovaný v jednořádkovém poli.
    queueMicrotask(() => {
      if (!area) return;
      autoResize(area);
      area.focus();
      // Kurzor na konec — text je k úpravě, ne k přepsání.
      area.setSelectionRange(area.value.length, area.value.length);
    });
  });

  return (
    <div class="composer">
      <div class="composer-bar">
        <button class="workspace-pick" onClick={vybratSlozku} title={props.workspacePath ?? ""}>
          <span class="workspace-icon">▣</span>
          <Show when={props.workspaceName} fallback={<span>Vybrat složku projektu</span>}>
            <span class="workspace-name">{props.workspaceName}</span>
          </Show>
        </button>

        <Show when={props.workspacePath}>
          <button
            class="ghost workspace-clear"
            onClick={() => props.onWorkspaceChange(null)}
            title="Zavřít složku"
          >
            ✕
          </button>
        </Show>

        <Show when={props.workspacePath}>
          <button
            class="review-btn"
            onClick={props.onReview}
            disabled={props.disabled}
            title="Nechat model projít projekt a najít problémy"
          >
            Zkontrolovat projekt
          </button>
        </Show>

        <div class="composer-spacer" />

        <Show when={props.contextTokens > 0}>
          <div class="context-meter" title={`${props.usedTokens} z ${props.contextTokens} tokenů`}>
            <div class="context-track">
              <div
                class="context-fill"
                classList={{ warn: zaplneni() >= 75 }}
                style={{ width: `${zaplneni()}%` }}
              />
            </div>
            <span class="context-label">kontext {zaplneni()} %</span>
          </div>
        </Show>
      </div>

      <div class="composer-input">
        <textarea
          ref={area}
          rows="1"
          placeholder={
            props.disabled
              ? "Nejdřív načti model v nastavení…"
              : "Napiš dotaz. Enter odešle, Shift+Enter je nový řádek."
          }
          value={text()}
          disabled={props.disabled}
          onInput={(e) => {
            setText(e.currentTarget.value);
            autoResize(e.currentTarget);
          }}
          onKeyDown={onKeyDown}
        />

        <Show
          when={props.generating}
          fallback={
            <button class="primary send" onClick={odeslat} disabled={props.disabled || !text().trim()}>
              Odeslat
            </button>
          }
        >
          <button class="send stop" onClick={props.onCancel}>
            Zastavit
          </button>
        </Show>
      </div>
    </div>
  );
}
