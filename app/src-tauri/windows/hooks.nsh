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
    Delete "$PLUGINSDIR\konak-stop.exe"
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
    DetailPrint "Konak cleanup: --uninstall$R9"
    ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall$R9' $R7
    DetailPrint "Konak cleanup finished with exit code $R7 (log in %TEMP%\konak-uninstall-*.log)"
  ${EndIf}
!macroend
