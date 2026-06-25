# powershell

Current session only:

```powershell
fermut completions powershell | Out-String | Invoke-Expression
```

Persist across sessions — append to your PowerShell profile:

```powershell
fermut completions powershell >> $PROFILE
```

If `$PROFILE` doesn't exist:

```powershell
New-Item -Path $PROFILE -ItemType File -Force
fermut completions powershell >> $PROFILE
```
