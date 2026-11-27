<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import { t } from "$lib/i18n";
  import { appName, type AppInfo, type Schedule, type Settings } from "./api";
  import BackupPassword from "./BackupPassword.svelte";
  import BackupRun from "./BackupRun.svelte";

  let {
    settings,
    apps,
    onsave,
    onreload,
    onrestore,
  }: {
    settings: Settings;
    apps: AppInfo[];
    onsave: (patch: (s: Settings) => Settings) => Promise<string | null>;
    onreload: () => void;
    onrestore: () => void;
  } = $props();

  let b = $derived(settings.backup);
  let chosenApps = $derived(b.include.apps.length > 0);
  let ready = $derived(!!b.location && b.password_set);
  const id = $props.id();
  const WEEKDAYS = [1, 2, 3, 4, 5, 6, 0];
  const field = "h-10 rounded-lg border border-control bg-surface px-3 text-sm focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-link";

  // Each part shows its own error next to its controls.
  let folderError = $state<string | null>(null);
  let scheduleError = $state<string | null>(null);
  let includeError = $state<string | null>(null);

  /** Saves a change to the backup settings, applied to the latest stored value. */
  const update = (patch: (b: Settings["backup"]) => Partial<Settings["backup"]>) =>
    onsave((s) => ({ ...s, backup: { ...s.backup, ...patch(s.backup) } }));

  async function chooseLocation() {
    const dir = await pickFolder({ directory: true, multiple: false, title: t("panel.settings.location_pick") });
    if (typeof dir === "string") folderError = await update(() => ({ location: dir }));
  }

  const clearLocation = async () => (folderError = await update(() => ({ location: null })));

  // An empty list means "all apps", so the last chosen app cannot be unticked here: the user picks
  // "All installed apps" for that instead.
  async function toggleApp(appId: string, input: HTMLInputElement) {
    const on = input.checked;
    if (!on && b.include.apps.length === 1 && b.include.apps[0] === appId) {
      input.checked = true;
      return;
    }
    includeError = await update((x) => ({
      include: { ...x.include, apps: on ? [...new Set([...x.include.apps, appId])] : x.include.apps.filter((a) => a !== appId) },
    }));
    if (includeError) input.checked = b.include.apps.includes(appId);
  }

  async function setFolder(key: "photos" | "media", input: HTMLInputElement) {
    const on = input.checked;
    includeError = await update((x) => ({ include: { ...x.include, [key]: on } }));
    if (includeError) input.checked = b.include[key];
  }

  const setAllApps = async (all: boolean) =>
    (includeError = await update((x) => ({ include: { ...x.include, apps: all ? [] : apps.map((a) => a.id) } })));

  // Whole numbers 1–100 only; anything else puts the stored value back in the field.
  async function setKeep(input: HTMLInputElement) {
    const n = Number(input.value);
    scheduleError = !Number.isInteger(n) || n < 1 || n > 100 ? t("errors.settings_keep") : await update(() => ({ keep: n }));
    if (scheduleError) input.value = String(b.keep);
  }

  // The schedule is edited in a local copy and saved a moment after the last change, so moving through
  // a list with the arrow keys (each step is a `change` in WebView2) saves once, not on every step.
  // svelte-ignore state_referenced_locally
  let schedule = $state<Schedule>({ ...settings.backup.schedule });
  let timer: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    const stored = b.schedule;
    if (timer === undefined) schedule = { ...stored };
  });

  function setSchedule(patch: Partial<Schedule>) {
    schedule = { ...schedule, ...patch };
    clearTimeout(timer);
    timer = setTimeout(async () => {
      const next = { ...schedule };
      scheduleError = await update(() => ({ schedule: next }));
      timer = undefined;
      if (scheduleError) schedule = { ...b.schedule };
    }, 600);
  }

  // The time is two lists, not <input type="time">, which follows the WebView's locale (AM/PM in
  // Turkish). Minutes come in 5-minute steps, plus the stored minute if it is not one of them.
  const HOURS = Array.from({ length: 24 }, (_, h) => String(h).padStart(2, "0"));
  let hour = $derived(schedule.time.split(":")[0] ?? "03");
  let minute = $derived(schedule.time.split(":")[1] ?? "00");
  let minutes = $derived([...new Set([...Array.from({ length: 12 }, (_, i) => String(i * 5).padStart(2, "0")), minute])].sort());
  const value = (e: Event) => (e.currentTarget as HTMLSelectElement).value;
</script>

<section aria-labelledby="backup-title" class="card gap-5">
  <div class="flex flex-col gap-1">
    <h2 id="backup-title" class="text-base font-semibold">{t("panel.settings.backup")}</h2>
    <p class="text-[13px] text-muted">{t("panel.settings.backup_intro")}</p>
  </div>

  <div class="flex flex-col gap-1.5">
    <p class="text-sm font-medium">{t("panel.settings.location")}</p>
    <div class="flex flex-wrap items-center gap-2">
      <span class="min-w-0 flex-1 truncate rounded-lg border border-border bg-raised px-3 py-2 font-mono text-[13px]" title={b.location ?? undefined}>{b.location ?? t("panel.settings.location_none")}</span>
      <button type="button" class="btn" onclick={chooseLocation}>{t("panel.settings.location_pick")}</button>
      {#if b.location}<button type="button" class="btn" onclick={clearLocation}>{t("panel.settings.location_clear")}</button>{/if}
    </div>
    <p class="text-xs text-muted">{t("panel.settings.location_hint")}</p>
    {#if folderError}<p class="text-xs text-error" role="alert">{folderError}</p>{/if}
  </div>

  <BackupPassword passwordSet={b.password_set} ondone={onreload} />

  <fieldset class="flex flex-wrap items-end gap-3">
    <legend class="mb-1.5 text-sm font-medium">{t("panel.settings.schedule")}</legend>
    <label class="flex flex-col gap-1 text-xs text-muted" for="{id}-kind">{t("panel.settings.schedule_how")}
      <select id="{id}-kind" class={field} value={schedule.kind} onchange={(e) => setSchedule({ kind: value(e) as Schedule["kind"] })}>
        <option value="off">{t("panel.settings.schedule_off")}</option>
        <option value="daily">{t("panel.settings.schedule_daily")}</option>
        <option value="weekly">{t("panel.settings.schedule_weekly")}</option>
      </select>
    </label>
    {#if schedule.kind === "weekly"}
      <label class="flex flex-col gap-1 text-xs text-muted" for="{id}-day">{t("panel.settings.schedule_day")}
        <select id="{id}-day" class={field} value={schedule.weekday} onchange={(e) => setSchedule({ weekday: Number(value(e)) })}>
          {#each WEEKDAYS as d (d)}<option value={d}>{t(`panel.settings.weekday_${d}`)}</option>{/each}
        </select>
      </label>
    {/if}
    {#if schedule.kind !== "off"}
      <fieldset class="flex flex-col gap-1 text-xs text-muted">
        <legend>{t("panel.settings.schedule_time")}</legend>
        <div class="flex items-center gap-1">
          <select aria-label={t("panel.settings.schedule_hour")} class={field} value={hour} onchange={(e) => setSchedule({ time: `${value(e)}:${minute}` })}>
            {#each HOURS as h (h)}<option value={h}>{h}</option>{/each}
          </select>
          <span aria-hidden="true">:</span>
          <select aria-label={t("panel.settings.schedule_minute")} class={field} value={minute} onchange={(e) => setSchedule({ time: `${hour}:${value(e)}` })}>
            {#each minutes as m (m)}<option value={m}>{m}</option>{/each}
          </select>
        </div>
      </fieldset>
    {/if}
    <label class="flex flex-col gap-1 text-xs text-muted" for="{id}-keep">{t("panel.settings.keep")}
      <input
        id="{id}-keep"
        type="number"
        min="1"
        max="100"
        step="1"
        class="{field} w-24"
        value={b.keep}
        aria-invalid={scheduleError ? "true" : undefined}
        onchange={(e) => setKeep(e.currentTarget as HTMLInputElement)}
      />
    </label>
    {#if schedule.kind !== "off" && !ready}<p class="w-full text-xs text-warn-text">{t("panel.settings.schedule_not_ready")}</p>{/if}
    {#if scheduleError}<p class="w-full text-xs text-error" role="alert">{scheduleError}</p>{/if}
  </fieldset>

  <fieldset class="flex flex-col gap-2">
    <legend class="mb-1.5 text-sm font-medium">{t("panel.settings.include")}</legend>
    <label class="flex items-center gap-2.5 text-sm">
      <input type="radio" name="{id}-apps" checked={!chosenApps} onchange={() => setAllApps(true)} class="size-4 accent-accent" />
      {t("panel.settings.include_all")}
    </label>
    <label class="flex items-center gap-2.5 text-sm">
      <input type="radio" name="{id}-apps" checked={chosenApps} disabled={apps.length === 0} onchange={() => setAllApps(false)} class="size-4 accent-accent" />
      {t("panel.settings.include_some")}
    </label>
    {#if chosenApps}
      <div class="ml-7 flex flex-wrap gap-x-5 gap-y-2">
        {#each apps as a (a.id)}
          <label class="flex items-center gap-2 text-sm">
            <input type="checkbox" checked={b.include.apps.includes(a.id)} onchange={(e) => toggleApp(a.id, e.currentTarget as HTMLInputElement)} class="size-4 accent-accent" />
            {appName(a.id)}
          </label>
        {/each}
      </div>
    {/if}
    <label class="mt-1 flex items-start gap-2.5 text-sm">
      <input type="checkbox" checked={b.include.photos} onchange={(e) => setFolder("photos", e.currentTarget as HTMLInputElement)} class="mt-0.5 size-4 accent-accent" />
      <span>{t("panel.settings.include_photos")}<span class="block text-xs text-muted">{t("panel.settings.include_photos_hint")}</span></span>
    </label>
    <label class="flex items-start gap-2.5 text-sm">
      <input type="checkbox" checked={b.include.media} onchange={(e) => setFolder("media", e.currentTarget as HTMLInputElement)} class="mt-0.5 size-4 accent-accent" />
      <span>{t("panel.settings.include_media")}<span class="block text-xs text-muted">{t("panel.settings.include_media_hint")}</span></span>
    </label>
    {#if includeError}<p class="text-xs text-error" role="alert">{includeError}</p>{/if}
  </fieldset>

  <BackupRun {ready} location={b.location} {onrestore} />
</section>
