import { For, Show } from "solid-js";

import type { ReviewReportView, Severity } from "../lib/api";

interface Props {
  report: ReviewReportView;
  onClose: () => void;
}

const SEVERITY_LABEL: Record<Severity, string> = {
  critical: "kritické",
  warning: "varování",
  note: "poznámka",
};

/**
 * Výpis nálezů z review.
 *
 * Nálezy jsou strukturovaná data, ne text vytažený z odpovědi — proto se dají
 * seřadit podle závažnosti a u každého je vidět soubor a řádek.
 */
export function Findings(props: Props) {
  const trvani = () => {
    const s = props.report.totalMs / 1000;
    return s < 60 ? `${s.toFixed(0)} s` : `${Math.floor(s / 60)} min ${Math.round(s % 60)} s`;
  };

  return (
    <section class="findings">
      <header class="findings-head">
        <div>
          <span class="findings-headline">{props.report.headline}</span>
          <span class="findings-meta">
            {props.report.filesRead.length} souborů · {props.report.rounds} kol · {trvani()}
          </span>
        </div>
        <button class="ghost" onClick={props.onClose} title="Skrýt nálezy">
          ✕
        </button>
      </header>

      {/* Bez tohohle nejde odlišit „nic nenašel" od „nedostal se k tomu". */}
      <Show when={props.report.hitRoundLimit}>
        <p class="findings-warning">
          Review skončilo na limitu kol, ne proto, že by model došel na konec.
          Projekt může mít víc problémů, než je vidět — zkus zúžit zadání na
          konkrétní část.
        </p>
      </Show>

      <Show
        when={props.report.findings.length > 0}
        fallback={
          <p class="findings-empty">
            Model nenahlásil žádný nález. Prošel{" "}
            {props.report.filesRead.length === 0
              ? "ale žádný soubor nepřečetl — zkus zadání zopakovat."
              : `${props.report.filesRead.length} souborů.`}
          </p>
        }
      >
        <ul class="findings-list">
          <For each={props.report.findings}>
            {(f) => (
              <li class={`finding finding-${f.severity}`}>
                <div class="finding-head">
                  <span class={`sev sev-${f.severity}`}>{SEVERITY_LABEL[f.severity]}</span>
                  <code class="finding-loc">{f.location}</code>
                </div>
                <p class="finding-summary">{f.summary}</p>
                <Show when={f.detail}>
                  <p class="finding-detail">{f.detail}</p>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </Show>

      <Show when={props.report.filesRead.length > 0}>
        <details class="findings-files">
          <summary>Přečtené soubory ({props.report.filesRead.length})</summary>
          <ul>
            <For each={props.report.filesRead}>{(f) => <li><code>{f}</code></li>}</For>
          </ul>
        </details>
      </Show>
    </section>
  );
}
