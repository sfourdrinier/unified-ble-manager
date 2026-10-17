// android/src/main/java/com/sfourdrinier/unifiedblemanager/AndroidBundleMetadata.java
package com.sfourdrinier.unifiedblemanager;

import android.os.Bundle;

import androidx.core.os.BundleCompat;

import java.io.Serializable;

/** Typed accessors for manifest metadata; absent values remain distinguishable from invalid ones. */
public final class AndroidBundleMetadata {
  private AndroidBundleMetadata() {}

  interface MetadataReader {
    boolean containsKey(String key);

    Serializable getSerializable(String key);
  }

  public static String optionalString(Bundle metadata, String key) {
    return optionalString(reader(metadata), key);
  }

  static String optionalString(MetadataReader metadata, String key) {
    if (!metadata.containsKey(key)) return null;
    final Serializable value = metadata.getSerializable(key);
    if (value == null) return null;
    if (!(value instanceof String)) {
      throw new IllegalStateException("Android metadata must be a string: " + key);
    }
    return (String) value;
  }

  public static String stringOrNull(Bundle metadata, String key) {
    return stringOrNull(reader(metadata), key);
  }

  static String stringOrNull(MetadataReader metadata, String key) {
    if (!metadata.containsKey(key)) return null;
    final Serializable value = metadata.getSerializable(key);
    return value instanceof String ? (String) value : null;
  }

  public static boolean booleanOrDefault(Bundle metadata, String key, boolean fallback) {
    return booleanOrDefault(reader(metadata), key, fallback);
  }

  static boolean booleanOrDefault(MetadataReader metadata, String key, boolean fallback) {
    if (!metadata.containsKey(key)) return fallback;
    final Serializable value = metadata.getSerializable(key);
    if (value instanceof String) return Boolean.parseBoolean((String) value);
    if (value instanceof Boolean) return (Boolean) value;
    return fallback;
  }

  private static MetadataReader reader(Bundle metadata) {
    return new MetadataReader() {
      @Override
      public boolean containsKey(String key) {
        return metadata.containsKey(key);
      }

      @Override
      public Serializable getSerializable(String key) {
        return BundleCompat.getSerializable(metadata, key, Serializable.class);
      }
    };
  }
}
