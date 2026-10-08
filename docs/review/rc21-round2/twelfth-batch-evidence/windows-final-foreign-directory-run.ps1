param([Parameter(Mandatory=$true)][string]$Candidate)
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
Set-Location ('C:\ubm-rc21-qualification\'+$Candidate)
$deadline=[DateTime]::UtcNow.AddSeconds(45)
while(-not (Test-Path 'foreign-ready.json') -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 250}
if(-not (Test-Path 'foreign-ready.json')){throw 'Foreign owner has not admitted its native link'}
try{
 Invoke-WebRequest 'http://192.168.122.1:18735/final-qualifiers/windows-final-foreign-directory.cjs' -OutFile 'windows-final-foreign-directory.cjs'
 foreach($runtime in @('C:\Program Files\nodejs\node.exe','C:\Users\admintest\.bun\bin\bun.exe')){
  & $runtime '.\windows-final-foreign-directory.cjs'
  if($LASTEXITCODE -ne 0){throw "Independent native directory failed: $runtime exit $LASTEXITCODE"}
 }
}finally{Set-Content -Encoding ASCII 'foreign-stop' 'qualification complete'}
