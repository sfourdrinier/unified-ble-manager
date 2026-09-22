// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/JsiStaticStubs.java

package com.sfourdrinier.unifiedblemanager.protocol;

import static org.mockito.ArgumentMatchers.any;
import static org.mockito.ArgumentMatchers.anyLong;
import static org.mockito.ArgumentMatchers.anyString;

import java.util.List;
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
}
