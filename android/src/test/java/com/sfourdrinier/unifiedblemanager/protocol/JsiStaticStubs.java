// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/JsiStaticStubs.java

package com.sfourdrinier.unifiedblemanager.protocol;

import static org.mockito.ArgumentMatchers.any;
import static org.mockito.ArgumentMatchers.anyLong;
import static org.mockito.ArgumentMatchers.anyString;
import static org.mockito.ArgumentMatchers.anyInt;
import static org.mockito.ArgumentMatchers.anyBoolean;

import java.util.List;
import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.io.IOException;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.mockito.MockedStatic;

/**
 * R02 authority test seam: stubs the {@code UnifiedBleProtocolJsiBinding}
 * statics (including its {@code native} methods, which have no library on
 * HOST-JVM) and captures what the dispatcher surfaces to JS.
 *
 * <p>Java on purpose: Kotlin cannot disambiguate
 * {@code MockedStatic.when(Verification)} from
 * {@code when(VerificationWithOutput)} for SAM lambdas, while Java overload
 * resolution (JLS 15.12.2.5) picks the value-bearing overload for
 * value-returning calls and the void overload for void calls.
 */
final class JsiStaticStubs {
  private JsiStaticStubs() {}

  /** Keeps the production C++ control registry in a host process, without JSI or a radio. */
  static final class Registry implements AutoCloseable {
    private final Process process;
    private final BufferedReader input;
    private final BufferedWriter output;

    Registry(String executable) throws IOException {
      process = new ProcessBuilder(executable).redirectError(ProcessBuilder.Redirect.INHERIT).start();
      input = new BufferedReader(new InputStreamReader(process.getInputStream()));
      output = new BufferedWriter(new OutputStreamWriter(process.getOutputStream()));
    }

    private synchronized String exchange(String line) throws IOException {
      output.write(line);
      output.newLine();
      output.flush();
      String answer = input.readLine();
      if (answer == null || answer.startsWith("error:")) throw new IOException("Native registry peer: " + answer);
      return answer;
    }

    private static String hex(byte[] bytes) {
      StringBuilder value = new StringBuilder(bytes.length * 2);
      for (byte b : bytes) value.append(String.format("%02x", b & 0xff));
      return value.toString();
    }

    void register(byte[] bytes) throws IOException {
      if (!exchange("R " + hex(bytes)).equals("registered")) throw new IOException("Native registration failed");
    }

    boolean settle(byte[] bytes) throws IOException { return exchange("S " + hex(bytes)).equals("settled"); }
    String cancel(long epoch, String nonce) throws IOException { return exchange("C " + epoch + " " + nonce); }
    boolean retire(long epoch, String nonce) throws IOException { return exchange("T " + epoch + " " + nonce).equals("retired"); }

    @Override public void close() throws Exception {
      synchronized (this) { output.write("Q\n"); output.flush(); }
      if (!process.waitFor(5, TimeUnit.SECONDS)) { process.destroyForcibly(); throw new IOException("Registry peer did not exit"); }
      if (process.exitValue() != 0) throw new IOException("Registry peer failed: " + process.exitValue());
    }
  }

  static void stubRegistry(MockedStatic<UnifiedBleProtocolJsiBinding> mocked, Registry registry, List<byte[]> emitted) {
    mocked.when(() -> UnifiedBleProtocolJsiBinding.emitRecord(anyLong(), any(byte[].class)))
        .thenAnswer(invocation -> {
          byte[] bytes = ((byte[]) invocation.getArgument(1)).clone();
          if (!registry.settle(bytes)) return false;
          emitted.add(bytes);
          return true;
        });
    mocked.when(() -> UnifiedBleProtocolJsiBinding.requestCancellation(anyLong(), anyLong(), anyString()))
        .thenAnswer(invocation -> registry.cancel(invocation.getArgument(1), invocation.getArgument(2)));
    mocked.when(() -> UnifiedBleProtocolJsiBinding.retireScan(anyLong(), anyLong(), anyString()))
        .thenAnswer(invocation -> registry.retire(invocation.getArgument(1), invocation.getArgument(2)));
  }

  static void stubEmitRecord(MockedStatic<UnifiedBleProtocolJsiBinding> mocked, List<byte[]> emitted) {
    mocked
        .when(() -> UnifiedBleProtocolJsiBinding.emitRecord(anyLong(), any(byte[].class)))
        .thenAnswer(
            invocation -> {
              emitted.add(((byte[]) invocation.getArgument(1)).clone());
              return true;
            });
  }

  static void stubEmitDiagnostic(
      MockedStatic<UnifiedBleProtocolJsiBinding> mocked, List<String> diagnostics) {
    mocked
        .when(
            () ->
                UnifiedBleProtocolJsiBinding.emitDiagnostic(
                    anyLong(), anyString(), anyString()))
        .thenAnswer(
            invocation -> {
              diagnostics.add(invocation.getArgument(1) + "|" + invocation.getArgument(2));
              return null;
            });
  }

  static void holdScanFailure(MockedStatic<UnifiedBleProtocolJsiBinding> mocked,
      CountDownLatch entered, CountDownLatch release, AtomicReference<byte[]> correlation) {
    mocked.when(() -> UnifiedBleProtocolJsiBinding.emitScanFailure(anyLong(), any(byte[].class), anyString()))
        .thenAnswer(invocation -> {
          correlation.set(((byte[]) invocation.getArgument(1)).clone());
          entered.countDown();
          if (!release.await(5, TimeUnit.SECONDS)) throw new AssertionError("scan failure was not released");
          return null;
        });
  }

  static void holdAdvertisement(MockedStatic<UnifiedBleProtocolJsiBinding> mocked,
      CountDownLatch entered, CountDownLatch release, AtomicReference<byte[]> correlation) {
    mocked.when(() -> UnifiedBleProtocolJsiBinding.emitAdvertisement(
        anyLong(), any(byte[].class), anyString(), any(), anyInt(), anyInt(), anyBoolean(),
        anyInt(), anyLong(), anyBoolean(), any(), any(), any(), any(), any(), any(), any()))
        .thenAnswer(invocation -> {
          correlation.set(((byte[]) invocation.getArgument(1)).clone());
          entered.countDown();
          if (!release.await(5, TimeUnit.SECONDS)) throw new AssertionError("advertisement was not released");
          return null;
        });
  }
}
