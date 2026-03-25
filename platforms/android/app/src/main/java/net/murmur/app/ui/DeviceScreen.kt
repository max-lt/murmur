package net.murmur.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import net.murmur.app.DeviceViewModel
import uniffi.murmur.DeviceInfoFfi

/**
 * Screen showing approved devices and pending join requests.
 *
 * - Pending requests: approve / deny buttons
 * - Approved devices: revoke button
 */
@Composable
fun DeviceScreen(viewModel: DeviceViewModel, myDeviceIdHex: String) {
    val devices by viewModel.devices.collectAsState()
    val pendingRequests by viewModel.pendingRequests.collectAsState()
    val error by viewModel.error.collectAsState()

    var revokeTarget by remember { mutableStateOf<DeviceInfoFfi?>(null) }

    // Other approved devices, excluding the current device.
    val otherApproved = devices.filter { it.approved && hex(it.deviceId) != myDeviceIdHex }

    Column(modifier = Modifier.padding(16.dp)) {

        error?.let {
            Text(it, color = MaterialTheme.colorScheme.error)
            Spacer(Modifier.height(8.dp))
        }

        if (pendingRequests.isNotEmpty()) {
            Text("Join Requests", style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.height(8.dp))
            LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                items(pendingRequests) { device ->
                    PendingDeviceCard(
                        device = device,
                        onApprove = { role ->
                            viewModel.approveDevice(
                                hex(device.deviceId),
                                role
                            )
                        }
                    )
                }
            }
            Spacer(Modifier.height(16.dp))
        }

        Text("Other Devices (${otherApproved.size})", style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.height(8.dp))
        if (otherApproved.isEmpty()) {
            Text("No other devices.", color = MaterialTheme.colorScheme.onSurfaceVariant)
        } else {
            LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                items(otherApproved) { device ->
                    ApprovedDeviceCard(
                        device = device,
                        onRevoke = { revokeTarget = device }
                    )
                }
            }
        }
    }

    revokeTarget?.let { target ->
        AlertDialog(
            onDismissRequest = { revokeTarget = null },
            title = { Text("Revoke ${target.name}?") },
            text = { Text("This will prevent ${target.name} from syncing.") },
            confirmButton = {
                Button(onClick = {
                    viewModel.revokeDevice(hex(target.deviceId))
                    revokeTarget = null
                }) { Text("Revoke") }
            },
            dismissButton = {
                TextButton(onClick = { revokeTarget = null }) { Text("Cancel") }
            }
        )
    }
}

@Composable
private fun PendingDeviceCard(
    device: DeviceInfoFfi,
    onApprove: (role: String) -> Unit
) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(12.dp)) {
            Text(device.name, style = MaterialTheme.typography.bodyLarge)
            Text(
                hex(device.deviceId).take(16) + "…",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
            Spacer(Modifier.height(8.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = { onApprove("full") }, modifier = Modifier.weight(1f)) {
                    Text("Approve (Full)")
                }
                OutlinedButton(onClick = { onApprove("source") }, modifier = Modifier.weight(1f)) {
                    Text("Source only")
                }
            }
        }
    }
}

@Composable
private fun ApprovedDeviceCard(
    device: DeviceInfoFfi,
    onRevoke: () -> Unit
) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier.padding(12.dp),
            horizontalArrangement = Arrangement.SpaceBetween
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(device.name, style = MaterialTheme.typography.bodyLarge)
                Text(
                    "Role: ${device.role}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
            }
            OutlinedButton(onClick = onRevoke) { Text("Revoke") }
        }
    }
}

private fun hex(bytes: ByteArray): String =
    bytes.joinToString("") { "%02x".format(it) }
