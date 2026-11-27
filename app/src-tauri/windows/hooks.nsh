; Tauri NSIS installer hooks (bundle.windows.nsis.installerHooks). Saved as UTF-8 with BOM so
; makensis reads the Turkish text correctly.

; Before the installer writes any file over an existing install: stop that install's Konak, caddy,
; mdns-responder and keepalive (`--stop`, src/uninstall.rs, exact path and command-line matching).
; Otherwise a running caddy.exe keeps its image locked and the file copy fails. It runs a copy of
; the new konak.exe from the plugins folder, so an older installed exe never sees a flag it does not
; know (it would start the app and ExecWait would never return).
!macro NSIS_HOOK_PREINSTALL
  ${If} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
    InitPluginsDir
    File "/oname=$PLUGINSDIR\konak-stop.exe" "${MAINBINARYSRCPATH}"
    DetailPrint "Stopping the running Konak"
    ExecWait '"$PLUGINSDIR\konak-stop.exe" --stop "$INSTDIR\${MAINBINARYNAME}.exe"' $R7
    DetailPrint "Konak stop finished with exit code $R7"
    ; 10 (uninstall::STOPPED_KONAK_EXIT): the Konak app was running; a silent update starts it again.
    Var /GLOBAL KonakWasRunning
    StrCpy $KonakWasRunning 0
    ${If} $R7 = 10
      StrCpy $KonakWasRunning 1
    ${EndIf}
    Delete "$PLUGINSDIR\konak-stop.exe"
    ; 3 (uninstall::REFUSED_EXIT): a backup, restore or startup recovery still runs; nothing was
    ; written yet, so the old install stays whole.
    ${If} $R7 = 3
      ${If} $LANGUAGE = 1055 ; Turkish
        StrCpy $R8 "Konak şu anda bir yedekleme, geri yükleme ya da başlangıç kurtarması yapıyor. Hiçbir şey değiştirilmedi.$\n$\nİşlem bitince kurulumu yeniden başlatın."
      ${Else}
        StrCpy $R8 "Konak is running a backup, a restore or a startup recovery right now. Nothing was changed.$\n$\nStart the installation again once it is done."
      ${EndIf}
      MessageBox MB_OK|MB_ICONEXCLAMATION "$R8" /SD IDOK
      SetErrorLevel 3
      ; Quit, not Abort: Abort leaves the window on its "aborted" page, which needs a second click.
      Quit
    ${EndIf}
  ${EndIf}
!macroend

; After a silent update: start Konak again (to the tray, as at sign-in) when the update stopped it.
; An interactive install offers this on its finish page.
!macro NSIS_HOOK_POSTINSTALL
  ${If} ${Silent}
  ${AndIf} $KonakWasRunning = 1
    Exec '"$INSTDIR\${MAINBINARYNAME}.exe" --autostart'
  ${EndIf}
!macroend

; Before the uninstaller removes any file: one question (delete all Konak data too? default No),
; then `konak.exe --uninstall [--purge]` (src/uninstall.rs) undoes what Konak set up outside its
; folder. Silent (/S) and passive (/P) runs take the default, No, without asking. Updates run the
; old uninstaller with /UPDATE; then nothing is cleaned up, so the distro, apps and settings stay.
!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode <> 1
    StrCpy $R9 ""
    ${If} $PassiveMode <> 1
      ${If} $LANGUAGE = 1055 ; Turkish
        StrCpy $R8 "Konak'ın tüm verileri de silinsin mi?$\n$\nKurulu uygulamalar ve içindeki her şey (dosyalar, parolalar, veritabanları) kalıcı olarak silinir. Hayır derseniz veriler kalır ve Konak yeniden kurulunca kaldığı yerden devam eder.$\n$\nKullanıcı klasörünüzdeki Konak klasöründe duran fotoğraflarınız ve medya dosyalarınız her iki durumda da silinmez."
      ${Else}
        StrCpy $R8 "Also delete all Konak data?$\n$\nInstalled apps and everything in them (files, passwords, databases) are deleted for good. If you choose No, the data stays and Konak picks up where it left off when you install it again.$\n$\nYour photos and media in the Konak folder under your user profile are kept either way."
      ${EndIf}
      ${If} ${Cmd} `MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "$R8" /SD IDNO IDYES`
        StrCpy $R9 " --purge"
      ${EndIf}
    ${EndIf}
    ; Silent: nobody answers the root-certificate dialog or a UAC prompt, so --silent skips those
    ; two steps (logged as left behind) instead of blocking ExecWait.
    ${If} ${Silent}
      StrCpy $R9 "$R9 --silent"
    ${EndIf}
    DetailPrint "Konak cleanup: --uninstall$R9 (waits up to 3 minutes for a running backup or restore)"
    ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall$R9' $R7
    DetailPrint "Konak cleanup finished with exit code $R7 (log in %TEMP%\konak-uninstall-*.log)"
    ; 3 (uninstall::REFUSED_EXIT): a backup or restore still runs and nothing was changed. Stop
    ; here, before Tauri's section deletes any file, so Konak and its uninstall entry stay whole.
    ; Silent runs end with exit code 3.
    ${If} $R7 = 3
      ${If} $LANGUAGE = 1055 ; Turkish
        StrCpy $R8 "Konak şu anda bir yedekleme veya geri yükleme yapıyor. Hiçbir şey kaldırılmadı.$\n$\nİşlem bitince (ya da yedeklemeyi Konak'ın Ayarlar sayfasından iptal edince) Konak'ı yeniden kaldırın."
      ${Else}
        StrCpy $R8 "Konak is running a backup or a restore right now. Nothing was removed.$\n$\nUninstall Konak again once it is done (or after cancelling the backup on Konak's Settings page)."
      ${EndIf}
      MessageBox MB_OK|MB_ICONEXCLAMATION "$R8" /SD IDOK
      SetErrorLevel 3
      ; Quit, not Abort: Abort leaves the window on its "aborted" page, which needs a second click.
      Quit
    ${EndIf}
  ${EndIf}
!macroend
