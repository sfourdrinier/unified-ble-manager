import dbus,dbus.service,json,time,os,signal,sys,subprocess
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib
DBusGMainLoop(set_as_default=True)
clients=[dbus.SystemBus(private=True) for _ in range(3)]
for b in clients:b.set_exit_on_disconnect(False)
ubm,reader,peripheral=clients
ctx=GLib.MainContext.default();a='/org/bluez/hci0';p=a+'/dev_DC_56_7B_D9_E8_A4';reverse='/org/bluez/hci1/dev_20_E1_5D_9E_A0_7F'
adv_path='/org/unifiedblemanager/QualificationAdvertisement'
registered=False;scanning=False;paused=False;token=None;auto_added=False
pid=int(sys.argv[1]);assert 'h10-sim' in open(f'/proc/{pid}/cmdline').read() and os.stat(f'/proc/{pid}').st_uid==os.getuid()
class Advertisement(dbus.service.Object):
 @dbus.service.method('org.freedesktop.DBus.Properties',in_signature='s',out_signature='a{sv}')
 def GetAll(self,interface):
  assert interface=='org.bluez.LEAdvertisement1'
  return dbus.Dictionary({'Type':dbus.String('peripheral'),'LocalName':dbus.String('UBM Prelease Probe')},signature='sv')
 @dbus.service.method('org.bluez.LEAdvertisement1',in_signature='',out_signature='')
 def Release(self):print(json.dumps({'phase':'advertisement-release'}),flush=True)
def pump_until(predicate,seconds,label):
 end=time.monotonic()+seconds
 while not predicate():
  while ctx.pending():ctx.iteration(False)
  if time.monotonic()>end:raise RuntimeError(label+' deadline')
  time.sleep(.01)
def pump_for(seconds):
 end=time.monotonic()+seconds
 while time.monotonic()<end:
  while ctx.pending():ctx.iteration(False)
  time.sleep(.01)
objects_api=dbus.Interface(ubm.get_object('org.bluez','/'),'org.freedesktop.DBus.ObjectManager')
def objects():return objects_api.GetManagedObjects(timeout=15)
def connected():return bool(objects().get(p,{}).get('org.bluez.Device1',{}).get('Connected',False))
adv_manager=dbus.Interface(peripheral.get_object('org.bluez',a),'org.bluez.LEAdvertisingManager1')
adapter=dbus.Interface(peripheral.get_object('org.bluez','/org/bluez/hci1'),'org.bluez.Adapter1')
lease=dbus.Interface(ubm.get_object('org.bluez',a),'org.unifiedblemanager.LELease1')
def failed(e):return {'error':str(e),'name':e.get_dbus_name()}
try:
 assert not connected(),'prelease control requires a new physical generation'
 policy_path='/sys/kernel/debug/bluetooth/hci0/device_list'
 before=subprocess.run(['sudo','cat',policy_path],capture_output=True,text=True,check=True).stdout
 assert 'dc:56:7b:d9:e8:a4' not in before.lower(),'controlled peer already has a kernel connection policy'
 add=subprocess.run(['sudo','btmgmt','--index','0','add-device','-a','2','-t','1','DC:56:7B:D9:E8:A4'],capture_output=True,text=True,timeout=15,check=True)
 auto_added=True
 print(json.dumps({'phase':'owned-kernel-auto-connect-policy','priorDeviceList':before,'addAnswer':add.stdout}),flush=True)
 pump_until(lambda:connected(),20,'locally initiated daemon-auto LE connection')
 pump_until(lambda:bool(objects().get(p,{}).get('org.bluez.Device1',{}).get('ServicesResolved',False)),25,'client service resolution')
 graph=objects();matches=[path for path,ifs in graph.items() if str(path).startswith(p+'/') and str(ifs.get('org.bluez.GattCharacteristic1',{}).get('UUID',''))=='00002a19-0000-1000-8000-00805f9b34fb'];assert len(matches)==1,matches
 os.kill(pid,signal.SIGSTOP);paused=True
 character=dbus.Interface(reader.get_object('org.bluez',matches[0]),'org.bluez.GattCharacteristic1');read_answer=[]
 character.ReadValue(dbus.Dictionary({},signature='sv'),reply_handler=lambda value:read_answer.append({'value':list(map(int,value))}),error_handler=lambda e:read_answer.append(failed(e)),timeout=20)
 pump_for(.3);assert not read_answer,'foreign read was not held'
 print(json.dumps({'phase':'foreign-read-before-first-lease','initiator':'kernel-auto-connect','centralAdapter':'hci0','peripheralAdapter':'hci1','heldProcess':pid,'readSender':reader.get_unique_name(),'pending':True}),flush=True)
 token=lease.ReserveLease(dbus.ObjectPath(p),dbus.UInt64(34586000),timeout=15);generation=int(lease.ConnectLease(token,timeout=15))
 receipt=lease.ReleaseLease(token,timeout=15);assert str(receipt[3])=='lease-released-protected',list(receipt)
 lease.AckLease(token,timeout=15);token=None
 pump_for(.5);assert connected() and not read_answer,'active foreign read lost its physical generation'
 os.kill(pid,signal.SIGCONT);paused=False
 pump_until(lambda:bool(read_answer),15,'accepted foreign read completion');assert 'value' in read_answer[0],read_answer
 pump_until(lambda:not connected(),15,'deferred physical reconciliation with live read sender')
 print(json.dumps({'result':'passed','generation':generation,'receipt':str(receipt[3]),'readAnswer':read_answer[0],'readSenderStillAlive':reader.get_unique_name(),'physicalReleased':True,'source':'installed daemon and actual ATT over two USB controllers; simulated server held by SIGSTOP'}),flush=True)
finally:
 if paused:os.kill(pid,signal.SIGCONT)
 if token is not None:
  try:lease.ReleaseLease(token,timeout=15);lease.AckLease(token,timeout=15)
  except Exception as e:print('lease cleanup:',repr(e),flush=True)
 if scanning:
  try:adapter.StopDiscovery(timeout=15)
  except Exception as e:print('discovery cleanup:',repr(e),flush=True)
 if registered:
  try:adv_manager.UnregisterAdvertisement(adv_path,timeout=15)
  except Exception as e:print('advertisement cleanup:',repr(e),flush=True)
 for b in clients:b.close()
 if auto_added:
  current=subprocess.run(['sudo','cat',policy_path],capture_output=True,text=True,check=True).stdout
  if 'dc:56:7b:d9:e8:a4' in current.lower():
   remove=subprocess.run(['sudo','btmgmt','--index','0','del-device','-t','1','DC:56:7B:D9:E8:A4'],capture_output=True,text=True,timeout=15,check=True)
   print(json.dumps({'phase':'owned-auto-policy-removed','answer':remove.stdout}),flush=True)
  final=subprocess.run(['sudo','cat',policy_path],capture_output=True,text=True,check=True).stdout
  assert final==before,{'priorDeviceList':before,'finalDeviceList':final}
  print(json.dumps({'phase':'kernel-auto-policy-restored','deviceList':final}),flush=True)

