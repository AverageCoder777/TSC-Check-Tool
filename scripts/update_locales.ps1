$ErrorActionPreference = "Stop"

if (-not (Get-Command xgettext -ErrorAction SilentlyContinue)) {
    $poeditPaths = @(
        "C:\Program Files\Poedit\GettextTools\bin",
        "C:\Program Files (x86)\Poedit\GettextTools\bin"
    )
    foreach ($dir in $poeditPaths) {
        if (Test-Path (Join-Path $dir "xgettext.exe")) {
            $env:PATH = "$dir;$env:PATH"
            break
        }
    }
}

New-Item -ItemType Directory -Force -Path locales | Out-Null

# 1. Slint
$slintFiles = Get-ChildItem -Path ui -Recurse -Filter *.slint | ForEach-Object { Resolve-Path -Relative $_.FullName }
slint-tr-extractor $slintFiles -o locales/slint.pot

# 2. Rust (убираем crate:: во временной копии, т.к. ':' ломает --keyword в xgettext)
$tmpSrc = "target/i18n_src"
if (Test-Path $tmpSrc) { Remove-Item -Recurse -Force $tmpSrc }
Copy-Item -Recurse -Force src $tmpSrc

$tmpRsFiles = Get-ChildItem -Path $tmpSrc -Recurse -Filter *.rs
foreach ($f in $tmpRsFiles) {
    $content = [System.IO.File]::ReadAllText($f.FullName)
    $updated = $content -replace '\bcrate::(n?gettext!)', '$1'
    if ($updated -ne $content) {
        [System.IO.File]::WriteAllText($f.FullName, $updated)
    }
}

$rsRelFiles = $tmpRsFiles | ForEach-Object { Resolve-Path -Relative $_.FullName }
xgettext --from-code=UTF-8 --language=Rust --keyword=gettext!:1 --keyword=ngettext!:1,2 --package-name=TSC_Check_Tool --output=locales/rust.pot $rsRelFiles

Remove-Item -Recurse -Force $tmpSrc

if (-not (Test-Path locales/rust.pot)) {
    throw "xgettext did not generate locales/rust.pot"
}

# 3. Склейка
msgcat --use-first locales/slint.pot locales/rust.pot -o locales/messages.pot
Remove-Item locales/slint.pot, locales/rust.pot

Write-Host "Done: locales/messages.pot updated."