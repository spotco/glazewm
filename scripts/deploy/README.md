# GlazeWM local deploy (no recurring UAC)

1. `backup_install.cmd` — copies current Program Files install to `%USERPROFILE%\glazewm-install-backups\<stamp>\`
2. **One-time UAC:** Run `grant_install_write_access.cmd` as Administrator (grants your user Modify on the install folder)
3. After each `build.bat`: `deploy_build.cmd` — stops GlazeWM and copies `target\release\glazewm*.exe` into the install dir (no UAC)

Restore a backup manually with robocopy from a stamp folder back to `C:\Program Files\glzr.io\GlazeWM`.
