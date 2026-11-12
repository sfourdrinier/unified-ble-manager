package com.ubm.core;

public class MobileCoreBridge {
    public static final class OrderedOpenIdentity {
        final int version, slot, itemCapacity, byteCapacity;
        final String owner, peer, connection, database, snapshot;
        final long generation, original, group, attachment, revision;
        final long[] members, services, characteristics;
        final String[] peers, serviceUuids, characteristicUuids;
        public OrderedOpenIdentity(int version, String owner, int slot, long generation, long original, long group, String peer,
                String connection, String database, String snapshot, long attachment, long revision,
                long[] members, String[] peers, String[] serviceUuids, long[] services, String[] characteristicUuids,
                long[] characteristics, int itemCapacity, int byteCapacity) {
            this.version = version; this.owner = owner; this.slot = slot; this.generation = generation;
            this.original = original; this.group = group; this.peer = peer; this.connection = connection;
            this.database = database; this.snapshot = snapshot; this.attachment = attachment; this.revision = revision;
            this.members = members; this.peers = peers; this.serviceUuids = serviceUuids; this.services = services;
            this.characteristicUuids = characteristicUuids; this.characteristics = characteristics;
            this.itemCapacity = itemCapacity; this.byteCapacity = byteCapacity;
        }
    }
    public static final class OrderedResult {
        public String kind, ownerIncarnation, status, ownershipKind, failureKind, detail, nativeName, nativeDomain, generationKind, generationValue;
        public int slot, gattStatus;
        public long slotGeneration;
        public long nativeCode;
        public boolean hasNativeCode, dispatched;
        public OrderedRegistration[] members;
    }
    public static final class OrderedRegistration {
        public long member, sourceMember, epoch;
    }
}
