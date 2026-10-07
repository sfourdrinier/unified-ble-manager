param([Parameter(Mandatory=$true)][string]$Candidate)
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$bleType=[Windows.Devices.Bluetooth.BluetoothLEDevice,Windows.Devices.Bluetooth,ContentType=WindowsRuntime]
$sessionType=[Windows.Devices.Bluetooth.GenericAttributeProfile.GattSession,Windows.Devices.Bluetooth,ContentType=WindowsRuntime]
$taskMethod=[System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.IsGenericMethod -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' } | Select-Object -First 1
function AwaitNative($operation,$type){$task=$taskMethod.MakeGenericMethod($type).Invoke($null,@($operation)); if(-not $task.Wait(30000)){throw 'Foreign native operation deadline'};return $task.Result}
Set-Location ('C:\ubm-rc21-qualification\'+$Candidate)
Remove-Item 'foreign-ready.json','foreign-stop' -ErrorAction SilentlyContinue
$device=$null;$session=$null;$services=$null
try{
 $device=AwaitNative ($bleType::FromBluetoothAddressAsync([Convert]::ToUInt64('DC567BD9E8A4',16))) $bleType
 $session=AwaitNative ($sessionType::FromDeviceIdAsync($device.BluetoothDeviceId)) $sessionType
 if(-not $session.CanMaintainConnection){throw 'Native session cannot maintain connection'}
 $session.MaintainConnection=$true
 $resultType=[Windows.Devices.Bluetooth.GenericAttributeProfile.GattDeviceServicesResult,Windows.Devices.Bluetooth,ContentType=WindowsRuntime]
 $services=AwaitNative ($device.GetGattServicesAsync()) $resultType
 if([string]$services.Status -ne 'Success'){throw "Foreign discovery status $($services.Status)"}
 $deadline=[DateTime]::UtcNow.AddSeconds(30)
 while([string]$device.ConnectionStatus -ne 'Connected' -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 100}
 if([string]$device.ConnectionStatus -ne 'Connected'){throw 'Foreign native link not connected'}
 $receipt=@{phase='foreign-native-owner-ready';address='DC:56:7B:D9:E8:A4';deviceId=$device.DeviceId;connected=[string]$device.ConnectionStatus;maintainConnection=$session.MaintainConnection;services=$services.Services.Count}
 $receipt | ConvertTo-Json -Compress | Set-Content -Encoding UTF8 'foreign-ready.json'
 $receipt | ConvertTo-Json -Compress
 $deadline=[DateTime]::UtcNow.AddMinutes(8)
 while(-not (Test-Path 'foreign-stop') -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 250}
 if(-not (Test-Path 'foreign-stop')){throw 'Foreign owner qualification lifetime exceeded'}
 if([string]$device.ConnectionStatus -ne 'Connected'){throw 'Directory operations ended the independent foreign connection'}
}finally{
 if($services){foreach($service in $services.Services){$service.Dispose()}}
 if($session){$session.MaintainConnection=$false;$session.Dispose()}
 if($device){$device.Dispose()}
 @{phase='foreign-native-owner-cleanup';sessionReleased=$true} | ConvertTo-Json -Compress
}
