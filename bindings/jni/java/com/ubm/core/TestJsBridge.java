package com.ubm.core;

import java.io.*;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.util.*;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.ConcurrentHashMap;

/** Test-only line bridge: JS drives the real JNI MobileSession and synthetic radio. */
public final class TestJsBridge {
    private static native void nativeEnableSyntheticMarker();
    private static final String PEER = "AA:BB:CC:DD:EE:FF";
    private static final List<MobileCoreBridge.OrderedRecord> records = Collections.synchronizedList(new ArrayList<>());
    private static final AtomicLong ordinal = new AtomicLong();
    private static final AtomicLong batches = new AtomicLong();
    private static volatile boolean cancelled;
    private static volatile boolean hold;
    private static volatile long heldRequest;
    private static volatile boolean holdPermitReservation;
    private static volatile boolean permitFenceSeen;
    private static volatile long heldPermitRequest;
    private static volatile String heldPermitKey;
    private static volatile long heldPermitId;
    private static volatile String permitKey;
    private static volatile long graphAuthority = -1;
    private static volatile boolean invalidatePermit;
    private static final Map<String, Long> permitRequests = new ConcurrentHashMap<>();
    private static final Set<String> forgottenPermits = ConcurrentHashMap.newKeySet();
    private static final Set<Long> authorityRequests = ConcurrentHashMap.newKeySet();
    private static final AtomicLong writes = new AtomicLong();
    private static final AtomicLong disables = new AtomicLong();
    private static String enc(String s) { return Base64.getEncoder().encodeToString(s.getBytes(StandardCharsets.UTF_8)); }
    private static String dec(String s) { return new String(Base64.getDecoder().decode(s), StandardCharsets.UTF_8); }
    private static synchronized void answer(String id, String value) { System.out.println(id + "\t" + enc(value)); System.out.flush(); }
    private static MobileCoreBridge.OrderedRegistration member(long id, long source, long epoch) { var m = new MobileCoreBridge.OrderedRegistration(); m.member=id; m.sourceMember=source; m.epoch=epoch; return m; }
    private static MobileCoreBridge.OrderedResult marker(String admission, String write, long n) { var r = new MobileCoreBridge.OrderedResult(); r.kind="marker-write-status"; r.admission=admission; r.writeState=write; r.ordinal=n; return r; }
    public static void main(String[] unused) throws Exception {
        TestMobile.Radio ordinary = new TestMobile.Radio();
        var radio = (MobileCoreBridge.RadioHost) Proxy.newProxyInstance(TestJsBridge.class.getClassLoader(), new Class<?>[]{MobileCoreBridge.RadioHost.class}, (proxy, method, a) -> {
            String name = method.getName(); long id = a == null || a.length == 0 || !(a[0] instanceof Number) ? 0 : ((Number)a[0]).longValue();
            switch (name) {
                case "orderedGraphAuthority": {
                    String op=(String)a[1]; long authority=(Long)a[2];
                    if(op.equals("reserve")) { authorityRequests.add(authority); var r=new MobileCoreBridge.OrderedResult(); r.kind="graph-authority-reserved"; r.authorityId=authority; MobileCoreBridge.nativeCompleteOrdered(id,r); }
                    else if(op.equals("reserve-fence") || op.equals("reserve-status")) { var r=new MobileCoreBridge.OrderedResult(); r.kind="graph-reserve-status"; r.status=authorityRequests.contains(authority)?"issued":"not-created"; r.authorityId=authority; MobileCoreBridge.nativeCompleteOrdered(id,r); }
                    else if(op.equals("reserve-forget")) { authorityRequests.remove(authority); MobileCoreBridge.nativeCompleteUnit(id); }
                    else if(op.equals("open")) { graphAuthority=authority; var r=new MobileCoreBridge.OrderedResult(); r.kind="graph-authority-opened"; r.authorityId=authority; MobileCoreBridge.nativeCompleteOrdered(id,r); }
                    else if((op.equals("status") || op.equals("fence-open"))) { var r=new MobileCoreBridge.OrderedResult(); r.kind="graph-authority-status"; r.status=graphAuthority==authority?"opened":"not-created"; r.authorityId=authority; MobileCoreBridge.nativeCompleteOrdered(id,r); }
                    else if(op.equals("forget-open")) MobileCoreBridge.nativeCompleteUnit(id);
                    else { if(authority!=graphAuthority) throw new AssertionError("authority identity mismatch"); if(op.equals("forget")) graphAuthority=-1; MobileCoreBridge.nativeCompleteUnit(id); }
                    return null;
                }
                case "orderedGraphPermitReservation": {
                    String op=(String)a[1]; String key=Arrays.deepToString(Arrays.copyOfRange(a,2,9)); long request=(Long)a[3];
                    if(op.equals("reserve")) { if(holdPermitReservation) { heldPermitRequest=id; heldPermitKey=key; heldPermitId=request; return null; } permitRequests.putIfAbsent(key,request); var r=new MobileCoreBridge.OrderedResult(); r.kind="graph-permit-reserved"; r.authorityId=permitRequests.get(key); MobileCoreBridge.nativeCompleteOrdered(id,r); }
                    else if(op.equals("status") || op.equals("fence")) { if(op.equals("fence") && key.equals(heldPermitKey)) permitFenceSeen=true; var r=new MobileCoreBridge.OrderedResult(); r.kind="graph-reserve-status"; r.status=permitRequests.containsKey(key)?"issued":key.equals(heldPermitKey)?"pending":"not-created"; r.authorityId=permitRequests.getOrDefault(key,0L); MobileCoreBridge.nativeCompleteOrdered(id,r); }
                    else { permitRequests.remove(key); MobileCoreBridge.nativeCompleteUnit(id); }
                    return null;
                }
                case "orderedGraphPermit": { String op=(String)a[1]; String key=Arrays.deepToString(Arrays.copyOfRange(a,2,9)); var r=new MobileCoreBridge.OrderedResult(); r.kind="graph-permit";
                    if(op.equals("acquire")) { if(!PEER.equals(a[2]) || !"graph".equals(a[4]) || (Long)a[5]!=1 || (Long)a[6]!=1 || (Long)a[7]!=graphAuthority) r.status="stale"; else if(permitKey!=null) r.status="busy"; else { permitKey=key; r.status="acquired"; } }
                    else if(op.equals("forget") && (forgottenPermits.contains(key) || (permitKey == null && permitRequests.containsValue((Long)a[8])))) { forgottenPermits.add(key); r.status="forgotten"; }
                    else if(!key.equals(permitKey)) r.status="stale";
                    else if(op.equals("commit") && invalidatePermit) { invalidatePermit=false; r.kind="graph-permit-invalidated"; r.failureKind="path-stale"; r.gattStatus=MobileCoreBridge.ABSENT_INT; r.detail="synthetic invalidation"; }
                    else if(op.equals("commit")) r.status="committed";
                    else if(op.equals("release")) r.status="released";
                    else { forgottenPermits.add(key); permitKey=null; r.status="forgotten"; }
                    MobileCoreBridge.nativeCompleteOrdered(id,r); return null; }
                case "discover": { MobileCoreBridge.nativeCompleteDiscoveredScoped(id, new int[]{0,1,2}, new String[]{"180d","2a37","2902"}, new long[]{0,0,0}, new int[]{0,0x1c,0}, "graph", 1, 1); return null; }
                case "orderedOpenV2":
                case "orderedRecoverOpenV2":
                case "orderedAcknowledgeOpenV2": {
                    var identity = (MobileCoreBridge.OrderedOpenIdentity)a[1];
                    var r = new MobileCoreBridge.OrderedResult();
                    r.kind = name.equals("orderedAcknowledgeOpenV2") ? "open-retired" : "open-result";
                    r.ownerIncarnation = identity.owner; r.slot = identity.slot; r.slotGeneration = identity.generation;
                    if (!name.equals("orderedAcknowledgeOpenV2")) {
                        r.status = "opened"; r.generationKind = "counter"; r.generationValue = "7";
                        r.members = new MobileCoreBridge.OrderedRegistration[identity.members.length];
                        for (int i = 0; i < identity.members.length; i++) r.members[i] = member(identity.members[i], 11+i, 2);
                    }
                    MobileCoreBridge.nativeCompleteOrdered(id, r); return null;
                }
                case "orderedOpen": { var r = new MobileCoreBridge.OrderedResult(); r.kind="opened"; r.generationKind="counter"; r.generationValue="7"; long[] members=(long[])a[2]; r.members=new MobileCoreBridge.OrderedRegistration[members.length]; for(int i=0;i<members.length;i++) r.members[i]=member(members[i], 11+i, 2); MobileCoreBridge.nativeCompleteOrdered(id,r); return null; }
                case "orderedEnable": { String op=(String)a[1]; if(op.equals("enable")) MobileCoreBridge.nativeCompleteNotifyEnabled(id,"notification"); else if(op.equals("disable")) { disables.incrementAndGet(); MobileCoreBridge.nativeCompleteUnit(id); } else if(op.equals("settlement")) { var r=new MobileCoreBridge.OrderedResult(); r.kind="enable-status"; r.status="enabled"; r.delivery="notification"; MobileCoreBridge.nativeCompleteOrdered(id,r); } else MobileCoreBridge.nativeCompleteUnit(id); return null; }
                case "orderedMarkerWrite": { long group=(Long)a[4], marker=(Long)a[5]; if(hold) { heldRequest=id; return null; } if(cancelled) { MobileCoreBridge.nativeCompleteOrdered(id,marker("not-admitted","not-started",0)); return null; } writes.incrementAndGet(); long n=ordinal.incrementAndGet(); var record=new MobileCoreBridge.OrderedRecord(); record.kind="marker"; record.markerId=marker; record.ordinal=n; records.add(record); MobileCoreBridge.nativeCompleteOrdered(id,marker("admitted","settled",n)); MobileCoreBridge.nativeIngestOrderedWake(7,group); return null; }
                case "orderedRequest": { String op=(String)a[1]; switch(op) {
                    case "retain-claim": { var r=new MobileCoreBridge.OrderedResult(); r.kind="claim"; r.generationKind="counter"; r.generationValue="7"; r.member=member((Long)a[5],(Long)a[6],(Long)a[7]); MobileCoreBridge.nativeCompleteOrdered(id,r); break; }
                    case "drain": { var r=new MobileCoreBridge.OrderedResult(); r.kind="records"; synchronized(records) { r.hasBatch=!records.isEmpty(); r.batch=r.hasBatch?batches.incrementAndGet():0; r.records=records.toArray(new MobileCoreBridge.OrderedRecord[0]); records.clear(); } MobileCoreBridge.nativeCompleteOrdered(id,r); break; }
                    case "barrier": { var barrier=new MobileCoreBridge.OrderedRecord(); barrier.kind="barrier"; barrier.barrierId=(Long)a[8]; records.add(barrier); MobileCoreBridge.nativeCompleteUnit(id); MobileCoreBridge.nativeIngestOrderedWake(7,(Long)a[4]); break; }
                    case "close": { var terminal=new MobileCoreBridge.OrderedRecord(); terminal.kind="terminal"; terminal.reason="closed"; records.add(terminal); MobileCoreBridge.nativeCompleteUnit(id); MobileCoreBridge.nativeIngestOrderedWake(7,(Long)a[4]); break; }
                    case "cancel-marker-write": if(hold || cancelled) { cancelled=true; MobileCoreBridge.nativeCompleteOrdered(id,marker("not-admitted","not-started",0)); } else MobileCoreBridge.nativeCompleteOrdered(id,marker("admitted","settled",ordinal.get())); break;
                    case "marker-write-settlement": MobileCoreBridge.nativeCompleteOrdered(id,cancelled?marker("not-admitted","not-started",0):hold?marker("pending","not-started",0):marker("admitted","settled",ordinal.get())); break;
                    default: MobileCoreBridge.nativeCompleteUnit(id); break;
                } return null; }
                default: try { return method.invoke(ordinary,a); } catch (InvocationTargetException e) { throw e.getCause(); }
            }
        });
        java.nio.file.Path directory=java.nio.file.Files.createTempDirectory("ubm-js-recording-");
        Runtime.getRuntime().addShutdownHook(new Thread(() -> { try (var paths = java.nio.file.Files.walk(directory)) { paths.sorted(Comparator.reverseOrder()).forEach(path -> { try { java.nio.file.Files.deleteIfExists(path); } catch (IOException error) { throw new UncheckedIOException(error); } }); } catch (IOException error) { throw new UncheckedIOException(error); } }));
        if (!MobileCoreBridge.nativeContinuationConfigureRecordingDirectory(directory.toString()).contains("\"ok\":true")) throw new IllegalStateException("recording directory setup failed");
        MobileCoreBridge.nativeInstallHost(radio, id -> {}, "android", "synthetic-js", "adapter");
        nativeEnableSyntheticMarker();
        String open=MobileCoreBridge.nativeOpenSession("js", "ubm-mobile-wire/3", "synthetic-js");
        long[] session={Long.parseLong(open.replaceAll(".*\\\"sessionId\\\":(\\d+).*", "$1"))};
        answer("ready",open);
        long admission=0;
        long[] previousSession={0};
        try (var in = new BufferedReader(new InputStreamReader(System.in,StandardCharsets.UTF_8))) {
            for(String line; (line=in.readLine())!=null;) {
                String[] p=line.split("\\t",3); String id=p[0], op=dec(p[1]), args=dec(p[2]);
                if(op.equals("_record-version")) { answer(id,Integer.toString(MobileCoreBridge.nativeOrderedRecordingVersion())); continue; }
                if(op.equals("_record-control")) { String[] fields=args.split("\u001f",-1); MobileCoreBridge.nativeOrderedRecordingReceiptControl(session[0],fields[0],fields[1],value -> answer(id,value)); continue; }
                if(op.equals("_record-start")) { String[] fields=args.split("\u001f",-1); MobileCoreBridge.nativeContinuationStartOrdered(session[0],fields[0],fields[1],fields[2],fields[3],fields[4],Long.parseLong(fields[5]),value -> answer(id,value)); continue; }
                if(op.equals("_record-stop")) { MobileCoreBridge.nativeContinuationStopOrdered(session[0],args,value -> answer(id,value)); continue; }
                if(op.equals("_advertise")) { MobileCoreBridge.nativeIngestAdvertisement(PEER,PEER,"fixture",-55,MobileCoreBridge.ABSENT_INT,new String[]{"180D"},new int[0],new byte[0][],new String[0],new byte[0][],0,null,new String[0],MobileCoreBridge.ABSENT_INT,null,null,0); answer(id,"{}"); continue; }
                if(op.equals("_drain")) { answer(id,MobileCoreBridge.nativeDrain(session[0],256,65536)); continue; }
                if(op.equals("_invalidate")) { invalidatePermit=true; answer(id,"{}"); continue; }
                if(op.equals("_reset")) { cancelled=false; hold=false; answer(id,"{}"); continue; }
                if(op.equals("_hold-permit")) { permitFenceSeen=false; holdPermitReservation=true; answer(id,"{}"); continue; }
                if(op.equals("_permit-fenced")) { answer(id,Boolean.toString(permitFenceSeen)); continue; }
                if(op.equals("_permit-held")) { answer(id,Long.toString(heldPermitRequest)); continue; }
                if(op.equals("_release-permit")) { holdPermitReservation=false; if(heldPermitRequest!=0) { permitRequests.put(heldPermitKey,heldPermitId); var result=new MobileCoreBridge.OrderedResult(); result.kind="graph-permit-reserved"; result.authorityId=heldPermitId; MobileCoreBridge.nativeCompleteOrdered(heldPermitRequest,result); heldPermitRequest=0; heldPermitKey=null; } answer(id,"{}"); continue; }
                if(op.equals("_hold")) { hold=true; cancelled=false; answer(id,"{}"); continue; }
                if(op.equals("_release")) { hold=false; long held=heldRequest; heldRequest=0; if(held!=0) MobileCoreBridge.nativeCompleteOrdered(held,marker("not-admitted","not-started",0)); answer(id,"{}"); continue; }
                if(op.equals("_held")) { answer(id,Long.toString(heldRequest)); continue; }
                if(op.equals("_disables")) { answer(id,Long.toString(disables.get())); continue; }
                if(op.equals("_writes")) { answer(id,Long.toString(writes.get())); continue; }
                if(op.equals("_retry-old")) { MobileCoreBridge.nativeInvoke(previousSession[0],"session.dispose","{}",value -> answer(id,value)); continue; }
                if(op.equals("_replace")) {
                    previousSession[0]=session[0];
                    MobileCoreBridge.nativeInvoke(session[0],"session.dispose","{}",value -> {
                        String replacement=MobileCoreBridge.nativeOpenSession("replacement", "ubm-mobile-wire/3", "synthetic-js");
                        session[0]=Long.parseLong(replacement.replaceAll(".*\"sessionId\":(\\d+).*", "$1"));
                        answer(id,value);
                    });
                    continue;
                }
                if(op.equals("_quit")) { answer(id,"{}"); break; }
                if(args.contains("\"operationId\"") && !op.equals("op.cancel") && !op.equals("scan.stop")) args="{\"admission\":"+(++admission)+","+args.substring(1);
                MobileCoreBridge.nativeInvoke(session[0],op,args,value -> answer(id,value));
            }
        }
        System.exit(0);
    }
}
