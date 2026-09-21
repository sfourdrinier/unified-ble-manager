// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/ReactCompanionChooserTest.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.bluetooth.le.ScanFilter
import android.companion.AssociationInfo
import android.companion.AssociationRequest
import android.companion.BluetoothDeviceFilter
import android.companion.BluetoothLeDeviceFilter
import android.companion.CompanionDeviceManager
import android.content.Context
import android.os.Build
import com.facebook.react.bridge.ReactApplicationContext
import java.util.regex.Pattern
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.ArgumentCaptor
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.anyBoolean
import org.mockito.ArgumentMatchers.anyInt
import org.mockito.Mockito.mock
import org.mockito.Mockito.mockConstruction
import org.mockito.Mockito.never
import org.mockito.Mockito.verify

/**
 * Finding 222: the companion chooser built the CLASSIC
 * `BluetoothDeviceFilter`, so a BLE-only peripheral (Polar H10) never
 * appeared in the system dialog. The association request must carry an LE
 * filter with the requested name pattern, and `setSingleDevice` only when a
 * name scopes the request (unscoped + single-device offers an arbitrary
 * device, which is how the wrong association happened).
 */
class ReactCompanionChooserTest {
  @Test
  fun associationRequestCarriesALeFilterWithTheExactNamePattern() {
    mockConstruction(
      BluetoothLeDeviceFilter.Builder::class.java,
      { builder, _ ->
        org.mockito.Mockito.doReturn(builder).`when`(builder).setNamePattern(any(Pattern::class.java))
        org.mockito.Mockito.doReturn(builder).`when`(builder).setScanFilter(any(ScanFilter::class.java))
        org.mockito.Mockito.doReturn(mock(BluetoothLeDeviceFilter::class.java)).`when`(builder).build()
      }
    ).use { leBuilders ->
      mockConstruction(BluetoothDeviceFilter.Builder::class.java).use { classicBuilders ->
        mockConstruction(
          AssociationRequest.Builder::class.java,
          { builder, _ ->
            org.mockito.Mockito.doReturn(builder).`when`(builder)
              .addDeviceFilter(any())
            org.mockito.Mockito.doReturn(builder).`when`(builder).setSingleDevice(anyBoolean())
            org.mockito.Mockito.doReturn(mock(AssociationRequest::class.java)).`when`(builder).build()
          }
        ).use { requestBuilders ->
          buildCompanionAssociationRequest("Polar H10 E9B93D29", null)

          // The request is built from an LE filter builder, never the classic one.
          assertTrue(leBuilders.constructed().isNotEmpty())
          assertTrue(classicBuilders.constructed().isEmpty())

          val patternCaptor = ArgumentCaptor.forClass(Pattern::class.java)
          verify(leBuilders.constructed().single()).setNamePattern(patternCaptor.capture())
          val pattern = patternCaptor.value
          assertTrue(pattern.matcher("Polar H10 E9B93D29").matches())
          assertFalse(pattern.matcher("S39 4BEA LE").matches())
          // Exact-name semantics: the bare prefix alone must not match, so the
          // example passes the strap's full advertised name.
          assertFalse(pattern.matcher("Polar H10").matches())

          val requestBuilder = requestBuilders.constructed().single()
          verify(requestBuilder).addDeviceFilter(any(BluetoothLeDeviceFilter::class.java))
          verify(requestBuilder).setSingleDevice(true)
        }
      }
    }
  }

  @Test
  fun unscopedAssociationListsLeDevicesInsteadOfSingleDevice() {
    mockConstruction(
      BluetoothLeDeviceFilter.Builder::class.java,
      { builder, _ ->
        org.mockito.Mockito.doReturn(mock(BluetoothLeDeviceFilter::class.java)).`when`(builder).build()
      }
    ).use { leBuilders ->
      mockConstruction(
        AssociationRequest.Builder::class.java,
        { builder, _ ->
          org.mockito.Mockito.doReturn(builder).`when`(builder)
            .addDeviceFilter(any())
          org.mockito.Mockito.doReturn(builder).`when`(builder).setSingleDevice(anyBoolean())
          org.mockito.Mockito.doReturn(mock(AssociationRequest::class.java)).`when`(builder).build()
        }
      ).use { requestBuilders ->
        buildCompanionAssociationRequest(null, null)

        assertTrue(leBuilders.constructed().isNotEmpty())
        verify(leBuilders.constructed().single(), never()).setNamePattern(any(Pattern::class.java))
        val requestBuilder = requestBuilders.constructed().single()
        verify(requestBuilder).addDeviceFilter(any(BluetoothLeDeviceFilter::class.java))
        verify(requestBuilder).setSingleDevice(false)
      }
    }
  }

  @Test
  fun serviceUuidScopingRidesTheLeScanFilter() {
    mockConstruction(
      BluetoothLeDeviceFilter.Builder::class.java,
      { builder, _ ->
        org.mockito.Mockito.doReturn(builder).`when`(builder).setScanFilter(any(ScanFilter::class.java))
        org.mockito.Mockito.doReturn(mock(BluetoothLeDeviceFilter::class.java)).`when`(builder).build()
      }
    ).use { leBuilders ->
      mockConstruction(
        AssociationRequest.Builder::class.java,
        { builder, _ ->
          org.mockito.Mockito.doReturn(builder).`when`(builder)
            .addDeviceFilter(any())
          org.mockito.Mockito.doReturn(builder).`when`(builder).setSingleDevice(anyBoolean())
          org.mockito.Mockito.doReturn(mock(AssociationRequest::class.java)).`when`(builder).build()
        }
      ).use {
        mockConstruction(
          ScanFilter.Builder::class.java,
          { builder, _ ->
            org.mockito.Mockito.doReturn(builder).`when`(builder)
              .setServiceUuid(any())
            org.mockito.Mockito.doReturn(mock(ScanFilter::class.java)).`when`(builder).build()
          }
        ).use { scanBuilders ->
          buildCompanionAssociationRequest(null, "0000180d-0000-1000-8000-00805f9b34fb")

          assertTrue(scanBuilders.constructed().isNotEmpty())
          verify(leBuilders.constructed().single()).setScanFilter(any(ScanFilter::class.java))
        }
      }
    }
  }

  /**
   * Finding 236: a named associate for an already-associated device reports
   * the existing record as already-associated and never reaches the system
   * chooser, so the OS cannot accumulate a duplicate.
   */
  @Test
  fun namedAssociateReportsTheExistingAssociationWithoutLaunchingTheChooser() {
    val reactContext = mock(ReactApplicationContext::class.java)
    val manager = mock(CompanionDeviceManager::class.java)
    org.mockito.Mockito.`when`(
      reactContext.getSystemService(Context.COMPANION_DEVICE_SERVICE)
    ).thenReturn(manager)
    val info = mock(AssociationInfo::class.java)
    org.mockito.Mockito.`when`(info.id).thenReturn(4)
    org.mockito.Mockito.`when`(info.displayName).thenReturn("Polar H10 E9B93D29")
    org.mockito.Mockito.`when`(manager.myAssociations).thenReturn(listOf(info))

    val chooser = ReactCompanionChooser(reactContext, Build.VERSION_CODES.TIRAMISU) { true }
    var result: Result<CompanionAssociation>? = null
    chooser.associate("Polar H10 E9B93D29", null) { result = it }

    val association = result!!.getOrThrow()
    org.junit.Assert.assertEquals(4L, association.associationId)
    assertTrue(association.alreadyAssociated)
    org.junit.Assert.assertEquals("Polar H10 E9B93D29", association.displayName)
    verify(manager, never()).associate(
      any(AssociationRequest::class.java),
      any(CompanionDeviceManager.Callback::class.java),
      any()
    )
  }

  @Test
  fun listingReportsEveryAssociation() {
    val manager = mock(CompanionDeviceManager::class.java)
    val infos = listOf(association(4, "Polar H10 E9B93D29"), association(5, "Polar H10 E9B93D29"))
    org.mockito.Mockito.`when`(manager.myAssociations).thenReturn(infos)
    val chooser = chooserFor(manager)

    val records = chooser.listAssociations()

    org.junit.Assert.assertEquals(2, records.size)
    org.junit.Assert.assertEquals(4L, records[0].associationId)
    org.junit.Assert.assertEquals("Polar H10 E9B93D29", records[1].displayName)
  }

  @Test
  fun disassociateRemovesTheKnownAssociation() {
    val manager = mock(CompanionDeviceManager::class.java)
    val infos = listOf(association(4, "Polar H10 E9B93D29"), association(5, "Polar H10 E9B93D29"))
    org.mockito.Mockito.`when`(manager.myAssociations).thenReturn(infos)
    val chooser = chooserFor(manager)

    chooser.disassociate(5)

    verify(manager).disassociate(5)
  }

  @Test
  fun disassociateOfAnUnknownAssociationReportsIt() {
    val manager = mock(CompanionDeviceManager::class.java)
    val infos = listOf(association(4, "Polar H10 E9B93D29"))
    org.mockito.Mockito.`when`(manager.myAssociations).thenReturn(infos)
    val chooser = chooserFor(manager)

    try {
      chooser.disassociate(9)
      org.junit.Assert.fail("expected an unknown association to be reported")
    } catch (error: RadioPortFailure) {
      org.junit.Assert.assertEquals("associationUnknown", error.nativeCode)
    }
    verify(manager, never()).disassociate(anyInt())
  }

  @Test
  fun unknownNameProceedsToTheSystemChooser() {
    val reactContext = mock(ReactApplicationContext::class.java)
    val manager = mock(CompanionDeviceManager::class.java)
    org.mockito.Mockito.`when`(
      reactContext.getSystemService(Context.COMPANION_DEVICE_SERVICE)
    ).thenReturn(manager)
    val info = mock(AssociationInfo::class.java)
    org.mockito.Mockito.`when`(info.id).thenReturn(4)
    org.mockito.Mockito.`when`(info.displayName).thenReturn("Polar H10 E9B93D29")
    org.mockito.Mockito.`when`(manager.myAssociations).thenReturn(listOf(info))

    // No Activity is attached, so the chooser UI cannot launch: reaching
    // this refusal proves the unknown name was not short-circuited.
    val chooser = ReactCompanionChooser(reactContext, Build.VERSION_CODES.TIRAMISU) { true }
    try {
      chooser.associate("Polar Verity Sense", null) {}
      org.junit.Assert.fail("expected the chooser to require a foreground Activity")
    } catch (error: RadioPortFailure) {
      org.junit.Assert.assertEquals("associationActivityUnavailable", error.nativeCode)
    }
  }

  private fun chooserFor(manager: CompanionDeviceManager): ReactCompanionChooser {
    val reactContext = mock(ReactApplicationContext::class.java)
    org.mockito.Mockito.`when`(
      reactContext.getSystemService(Context.COMPANION_DEVICE_SERVICE)
    ).thenReturn(manager)
    return ReactCompanionChooser(reactContext, Build.VERSION_CODES.TIRAMISU) { true }
  }

  private fun association(id: Int, displayName: String): AssociationInfo {
    val info = mock(AssociationInfo::class.java)
    org.mockito.Mockito.`when`(info.id).thenReturn(id)
    org.mockito.Mockito.`when`(info.displayName).thenReturn(displayName)
    return info
  }
}
