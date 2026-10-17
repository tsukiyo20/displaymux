; NSIS installer for displaymux.
;
; Per-user, like the MSI: no UAC prompt, and the DDC helper lands next to the
; GUI, which is where it looks for it.
;
;   makensis /DVERSION=0.1.0 /DBINDIR=... /DOUTFILE=... installer.nsi

!include "MUI2.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef BINDIR
  !define BINDIR "."
!endif
!ifndef OUTFILE
  !define OUTFILE "displaymux-setup.exe"
!endif

Name "displaymux"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\displaymux"
InstallDirRegKey HKCU "Software\tsukiyo20\displaymux" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
Unicode true

!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\displaymux"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "displaymux" SecMain
  SetOutPath "$INSTDIR"

  ; The updater replaces the running executable by renaming it aside, so the
  ; three binaries have to stay together in one directory.
  File "${BINDIR}\dmux.exe"
  File "${BINDIR}\dmux-gui.exe"
  File "${BINDIR}\dmux-ddc-helper.exe"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\displaymux"
  CreateShortcut "$SMPROGRAMS\displaymux\displaymux.lnk" "$INSTDIR\dmux-gui.exe"
  CreateShortcut "$SMPROGRAMS\displaymux\Uninstall.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "Software\tsukiyo20\displaymux" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "displaymux"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\dmux-gui.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "tsukiyo20"
  WriteRegStr HKCU "${UNINST_KEY}" "URLInfoAbout" "https://github.com/tsukiyo20/displaymux"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\dmux.exe"
  Delete "$INSTDIR\dmux-gui.exe"
  Delete "$INSTDIR\dmux-ddc-helper.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\displaymux\displaymux.lnk"
  Delete "$SMPROGRAMS\displaymux\Uninstall.lnk"
  RMDir "$SMPROGRAMS\displaymux"

  ; Settings and port labels are the user's, not the installer's: leave
  ; %APPDATA%\displaymux alone.
  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "Software\tsukiyo20\displaymux"
SectionEnd
