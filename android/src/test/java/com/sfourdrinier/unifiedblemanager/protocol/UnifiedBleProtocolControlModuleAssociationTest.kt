// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModuleAssociationTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import android.bluetooth.le.ScanFilter
import android.companion.AssociationRequest
import android.companion.BluetoothDeviceFilter
import android.companion.BluetoothLeDeviceFilter
import java.util.regex.Pattern
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.ArgumentCaptor
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.anyBoolean
import org.mockito.Mockito.mock
import org.mockito.Mockito.mockConstruction
import org.mockito.Mockito.never
import org.mockito.Mockito.verify

/**
 * Finding 222 twin (FXH): the legacy protocol-control association built the
 * CLASSIC `BluetoothDeviceFilter` with `setSingleDevice(true)`, so a BLE-only
 * peripheral never matched and an unscoped request confirmed an arbitrary
 * device. It gets the same LE treatment as the Rust-route chooser: an LE
 * filter with the exact name pattern, service-UUID scoping on the LE scan
 * filter, and single-device confirmation only for named requests.
 */
class UnifiedBleProtocolControlModuleAssociationTest {
  @Test
  fun namedAssociationCarriesALeFilterWithTheExactNamePattern() {
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
          LegacyCompanionAssociationRequests.build("Polar H10 E9B93D29", null)

          assertTrue(leBuilders.constructed().isNotEmpty())
          assertTrue(classicBuilders.constructed().isEmpty())

          val patternCaptor = ArgumentCaptor.forClass(Pattern::class.java)
          verify(leBuilders.constructed().single()).setNamePattern(patternCaptor.capture())
          val pattern = patternCaptor.value
          assertTrue(pattern.matcher("Polar H10 E9B93D29").matches())
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
        LegacyCompanionAssociationRequests.build(null, null)

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
          LegacyCompanionAssociationRequests.build(null, "0000180d-0000-1000-8000-00805f9b34fb")

          assertTrue(scanBuilders.constructed().isNotEmpty())
          verify(leBuilders.constructed().single()).setScanFilter(any(ScanFilter::class.java))
        }
      }
    }
  }
}
