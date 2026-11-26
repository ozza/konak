<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import { t } from "$lib/i18n";
  import { appName, type AppInfo, type Settings } from "./api";
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
    onsave: (next: Settings) => Promise<boolean>;
    onreload: () => void;
    onrestore: () => void;
  } = $props();

  let b = $derived(settings.backup);
  let chosenApps = $derived(b.include.apps.length > 0);
  const id = $props.id();
  const WEEKDAYS = [1, 2, 3, 4, 5, 6, 0];
  const field = "h-10 rounded-lg border border-control bg-surface px-3 text-sm focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-link";

  function update(patch: Partial<Settings["backup"]>) {
    return onsave({ ...settings, backup: { ...b, ...patch } });
  }

  async function chooseLocation() {
    const dir = await pickFolder({ directory: true, multiple: false, title: t("panel.settings.location_pick") });
    if (typeof dir === "string") update({ location: dir });
  }

  function toggleApp(appId: string, on: boolean) {
    const list = on ? [...b.include.apps, appId] : b.include.apps.filter((a) => a !== appId);
    update({ include: { ...b.include, apps: list } });
  }
</script>

<section aria-labelledby="backup-title" class="card gap-5">
  <div class="flex flex-col gap-1">
    <h2 id="backup-title" class="text-base font-semibold">{t("panel.settings.backup")}</h2>
    <p class="text-[13px] text-muted">{t("panel.settings.backup_intro")}</p>
  </div>

  <div class="flex flex-col gap-1.5">
    <p class="text-sm font-medium">{t("panel.settings.location")}</p>
    <div class="flex flex-wrap items-center gap-2">
      <span class="min-w-0 flex-1 truncate rounded-lg border border-border bg-raised px-3 py-2 font-mono text-[13px]">{b.location ?? t("panel.settings.location_none")}</span>
      <button type="button" class="btn" onclick={chooseLocation}>{t("panel.settings.location_pick")}</button>
    </div>
    <p class="text-xs text-muted">{t("panel.settings.location_hint")}</p>
  </div>

  <BackupPassword passwordSet={b.password_set} ondone={onreload} />

  <fieldset class="flex flex-wrap items-end gap-3">
    <legend class="mb-1.5 text-sm font-medium">{t("panel.settings.schedule")}</legend>
    <label class="flex flex-col gap-1 text-xs text-muted" for="{id}-kind">{t("panel.settings.schedule_how")}
      <select id="{id}-kind" class={field} value={b.schedule.kind} onchange={(e) => update({ schedule: { ...b.schedule, kind: (e.currentTarget as HTMLSelectElement).value as Settings["backup"]["schedule"]["kind"] } })}>
        <option value="off">{t("panel.settings.schedule_off")}</option>
        <option value="daily">{t("panel.settings.schedule_daily")}</option>
        <option value="weekly">{t("panel.settings.schedule_weekly")}</option>
      </select>
    </label>
    {#if b.schedule.kind === "weekly"}
      <label class="flex flex-col gap-1 text-xs text-muted" for="{id}-day">{t("panel.settings.schedule_day")}
        <select id="{id}-day" class={field} value={b.schedule.weekday} onchange={(e) => update({ schedule: { ...b.schedule, weekday: Number((e.currentTarget as HTMLSelectElement).value) } })}>
          {#each WEEKDAYS as d (d)}<option value={d}>{t(`panel.settings.weekday_${d}`)}</option>{/each}
        </select>
      </label>
    {/if}
    {#if b.schedule.kind !== "off"}
      <label class="flex flex-col gap-1 text-xs text-muted" for="{id}-time">{t("panel.settings.schedule_time")}
        <input id="{id}-time" type="time" class={field} value={b.schedule.time} onchange={(e) => update({ schedule: { ...b.schedule, time: (e.currentTarget as HTMLInputElement).value } })} />
      </label>
    {/if}
    <label class="flex flex-col gap-1 text-xs text-muted" for="{id}-keep">{t("panel.settings.keep")}
      <input id="{id}-keep" type="number" min="1" max="100" class="{field} w-24" value={b.keep} onchange={(e) => update({ keep: Number((e.currentTarget as HTMLInputElement).value) })} />
    </label>
  </fieldset>

  <fieldset class="flex flex-col gap-2">
    <legend class="mb-1.5 text-sm font-medium">{t("panel.settings.include")}</legend>
    <label class="flex items-center gap-2.5 text-sm">
      <input type="radio" name="{id}-apps" checked={!chosenApps} onchange={() => update({ include: { ...b.include, apps: [] } })} class="size-4 accent-accent" />
      {t("panel.settings.include_all")}
    </label>
    <label class="flex items-center gap-2.5 text-sm">
      <input type="radio" name="{id}-apps" checked={chosenApps} disabled={apps.length === 0} onchange={() => update({ include: { ...b.include, apps: apps.map((a) => a.id) } })} class="size-4 accent-accent" />
      {t("panel.settings.include_some")}
    </label>
    {#if chosenApps}
      <div class="ml-7 flex flex-wrap gap-x-5 gap-y-2">
        {#each apps as a (a.id)}
          <label class="flex items-center gap-2 text-sm">
            <input type="checkbox" checked={b.include.apps.includes(a.id)} onchange={(e) => toggleApp(a.id, (e.currentTarget as HTMLInputElement).checked)} class="size-4 accent-accent" />
            {appName(a.id)}
          </label>
        {/each}
      </div>
    {/if}
    <label class="mt-1 flex items-start gap-2.5 text-sm">
      <input type="checkbox" checked={b.include.photos} onchange={(e) => update({ include: { ...b.include, photos: (e.currentTarget as HTMLInputElement).checked } })} class="mt-0.5 size-4 accent-accent" />
      <span>{t("panel.settings.include_photos")}<span class="block text-xs text-muted">{t("panel.settings.include_photos_hint")}</span></span>
    </label>
    <label class="flex items-start gap-2.5 text-sm">
      <input type="checkbox" checked={b.include.media} onchange={(e) => update({ include: { ...b.include, media: (e.currentTarget as HTMLInputElement).checked } })} class="mt-0.5 size-4 accent-accent" />
      <span>{t("panel.settings.include_media")}<span class="block text-xs text-muted">{t("panel.settings.include_media_hint")}</span></span>
    </label>
  </fieldset>

  <BackupRun ready={!!b.location && b.password_set} {onrestore} />
</section>
