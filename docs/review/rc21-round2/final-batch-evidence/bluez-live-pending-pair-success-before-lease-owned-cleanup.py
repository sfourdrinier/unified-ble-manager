import dbus,dbus.service,json,time,sys
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib
DBusGMainLoop(set_as_default=True)
mode=sys.argv[1] if len(sys.argv)>1 else 'refuse'
assert mode in ['refuse','cancel','sender-death','pair-opens-link','success','success-before-lease']
foreign_closed=False
ubm=dbus.SystemBus(private=True);foreign=dbus.SystemBus(private=True)
ubm.set_exit_on_disconnect(False);foreign.set_exit_on_disconnect(False)
a='/org/bluez/hci0';p=a+'/dev_DC_56_7B_D9_E8_A4'
lease=dbus.Interface(ubm.get_object('org.bluez',a),'org.unifiedblemanager.LELease1')
objects_api=dbus.Interface(ubm.get_object('org.bluez','/'),'org.freedesktop.DBus.ObjectManager')
adapter=dbus.Interface(foreign.get_object('org.bluez',a),'org.bluez.Adapter1')
agent_manager=dbus.Interface(foreign.get_object('org.bluez','/org/bluez'),'org.bluez.AgentManager1')
agent_path='/org/unifiedblemanager/QualificationHeldAgent'
ctx=GLib.MainContext.default();pending=[];answer=[];token=None;registered=False;scanning=False
class HeldAgent(dbus.service.Object):
 @dbus.service.method('org.bluez.Agent1',in_signature='o',out_signature='',async_callbacks=('ok','bad'))
 def RequestAuthorization(self,device,ok,bad):
  pending.append((str(device),ok,bad));print(json.dumps({'phase':'authorization-held','device':str(device)}),flush=True)
 @dbus.service.method('org.bluez.Agent1',in_signature='ou',out_signature='',async_callbacks=('ok','bad'))
 def RequestConfirmation(self,device,passkey,ok,bad):
  pending.append((str(device),ok,bad));print(json.dumps({'phase':'confirmation-held','device':str(device)}),flush=True)
 @dbus.service.method('org.bluez.Agent1',in_signature='',out_signature='')
 def Cancel(self): print(json.dumps({'phase':'agent-cancel'}),flush=True)
 @dbus.service.method('org.bluez.Agent1',in_signature='',out_signature='')
 def Release(self): print(json.dumps({'phase':'agent-release'}),flush=True)
def objects():return objects_api.GetManagedObjects(timeout=15)
def connected():return bool(objects().get(p,{}).get('org.bluez.Device1',{}).get('Connected',False))
def pump_until(predicate,seconds,label):
 end=time.monotonic()+seconds
 while not predicate():
  while ctx.pending():ctx.iteration(False)
  if time.monotonic()>end:raise RuntimeError(label+' deadline')
  time.sleep(.01)
def settle():
 end=time.monotonic()+2
 while time.monotonic()<end:
  while ctx.pending():ctx.iteration(False)
  time.sleep(.01)
try:
 adapter.SetDiscoveryFilter(dbus.Dictionary({'Transport':dbus.String('le')},signature='sv'),timeout=15)
 adapter.StartDiscovery(timeout=15);scanning=True
 pump_until(lambda:p in objects(),20,'peripheral discovery')
 assert not bool(objects()[p]['org.bluez.Device1'].get('Paired',False)),'test requires an unpaired peripheral; no bonds will be deleted'
 agent=HeldAgent(foreign,agent_path)
 agent_manager.RegisterAgent(agent_path,'NoInputNoOutput',timeout=15);registered=True
 agent_manager.RequestDefaultAgent(agent_path,timeout=15)
 if mode not in ['pair-opens-link','success-before-lease']:
  token=lease.ReserveLease(dbus.ObjectPath(p),dbus.UInt64(34585000),timeout=15)
  generation=int(lease.ConnectLease(token,timeout=25))
 device=dbus.Interface(foreign.get_object('org.bluez',p),'org.bluez.Device1')
 device.Pair(reply_handler=lambda:answer.append({'kind':'success'}),error_handler=lambda e:answer.append({'kind':'error','name':e.get_dbus_name(),'message':str(e)}),timeout=30)
 pump_until(lambda:bool(pending) or bool(answer),15,'accepted pending Pair agent admission')
 assert pending and not answer,('Pair did not remain accepted and pending',answer)
 if mode in ['pair-opens-link','success-before-lease']:
  token=lease.ReserveLease(dbus.ObjectPath(p),dbus.UInt64(34585001),timeout=15)
  generation=int(lease.ConnectLease(token,timeout=25))
 assert connected()
 receipt=lease.ReleaseLease(token,timeout=15)
 assert str(receipt[3])=='lease-released-protected',list(receipt)
 lease.AckLease(token,timeout=15);token=None
 settle();assert connected() and not answer,'accepted pending Pair lost its physical link'
 print(json.dumps({'phase':'pending-pair-protected','generation':generation,'receipt':str(receipt[3]),'connected':True,'foreignSender':foreign.get_unique_name(),'agentDevice':pending[0][0]}),flush=True)
 if mode in ['success','success-before-lease']:
  pending.pop(0)[1]()
  pump_until(lambda:bool(answer),15,'Pair success completion')
  assert answer[0]['kind']=='success',answer
  assert bool(objects()[p]['org.bluez.Device1'].get('Paired',False))
  assert connected(),'successful foreign Pair must retain its committed interest'
  print(json.dumps({'phase':'pair-success-committed','connected':True,'paired':True,'foreignSender':foreign.get_unique_name()}),flush=True)
  if mode=='success-before-lease':
   settle();assert connected(),'the library must preserve the foreign-initiated link after Pair success'
   device.Disconnect(timeout=15)
   pump_until(lambda:not connected(),15,'explicit foreign-owner fixture cleanup')
   print(json.dumps({'phase':'foreign-origin-explicit-owner-cleanup','sender':foreign.get_unique_name(),'libraryForcedDisconnect':False}),flush=True)
  adapter.StopDiscovery(timeout=15);scanning=False
  agent_manager.UnregisterAgent(agent_path,timeout=15);registered=False
  foreign.close();foreign_closed=True
 elif mode=='sender-death':
  foreign.close();foreign_closed=True;registered=False;scanning=False;pending.clear()
  answer.append({'kind':'sender-death'})
 elif mode=='cancel':
  device.CancelPairing(timeout=15)
  pump_until(lambda:bool(answer),15,'Pair cancellation completion')
  assert answer[0]['kind']=='error',answer
  pending.clear()
 else:
  pending.pop(0)[2](dbus.exceptions.DBusException('qualification refuses ceremony after protection check',name='org.bluez.Error.Rejected'))
  pump_until(lambda:bool(answer),15,'Pair refusal completion')
  assert answer[0]['kind']=='error',answer
 pump_until(lambda:not connected(),15,'Pair interest retirement reconciliation')
 print(json.dumps({'result':'passed','pairOutcome':answer[0],'mode':mode,'physicalReleasedAfterRetirement':mode!='success-before-lease','physicalReleaseCause':'explicit-foreign-owner-cleanup' if mode=='success-before-lease' else 'deferred-library-cleanup','foreignSenderStillAlive':not foreign_closed,'controller':'hci0 with hci1 simulated peripheral'}),flush=True)
finally:
 if pending:
  try:pending.pop(0)[2](dbus.exceptions.DBusException('qualification cleanup',name='org.bluez.Error.Rejected'))
  except Exception as e:print('agent response cleanup:',repr(e),flush=True)
 if token is not None:
  try:lease.ReleaseLease(token,timeout=15);lease.AckLease(token,timeout=15)
  except Exception as e:print('lease cleanup:',repr(e),flush=True)
 if registered:
  try:agent_manager.UnregisterAgent(agent_path,timeout=15)
  except Exception as e:print('agent cleanup:',repr(e),flush=True)
 if scanning:
  deadline=time.monotonic()+3
  while True:
   try:adapter.StopDiscovery(timeout=15);break
   except dbus.exceptions.DBusException as e:
    if e.get_dbus_name()!='org.bluez.Error.InProgress' or time.monotonic()>deadline:print('discovery cleanup:',repr(e),flush=True);break
    settle()
 if not foreign_closed:foreign.close()
 ubm.close()
