package net.murmur.app.ui

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.Bundle
import android.os.IBinder
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Devices
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.LinkOff
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import net.murmur.app.DeviceViewModel
import net.murmur.app.FileViewModel
import net.murmur.app.MurmurEngine
import net.murmur.app.MurmurService

/**
 * Single-activity host.  Uses Jetpack Compose for all UI.
 *
 * Tabs:
 *  - **Devices** — approve/revoke, list all devices
 *  - **Files**   — browse and upload synced files
 *  - **Status**  — device ID, DAG info, event log
 */
@OptIn(ExperimentalMaterial3Api::class)
class MainActivity : ComponentActivity() {

    private var murmurService: MurmurService? = null
    private var serviceEngine by mutableStateOf<MurmurEngine?>(null)

    private val serviceConnection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val localBinder = binder as MurmurService.LocalBinder
            murmurService = localBinder.getService()
            serviceEngine = localBinder.getEngine()
        }

        override fun onServiceDisconnected(name: ComponentName) {
            murmurService = null
            serviceEngine = null
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Start + bind to the service.
        val serviceIntent = Intent(this, MurmurService::class.java)
        startForegroundService(serviceIntent)
        bindService(serviceIntent, serviceConnection, Context.BIND_AUTO_CREATE)

        setContent {
            MurmurTheme {
                var selectedTab by remember { mutableIntStateOf(0) }
                var showDisconnectDialog by remember { mutableStateOf(false) }

                val engine = serviceEngine
                var initialized by remember { mutableStateOf(
                    getSharedPreferences("murmur", Context.MODE_PRIVATE).contains("mnemonic")
                ) }
                val deviceName = remember(initialized) {
                    murmurService?.getDeviceName()
                        ?: getSharedPreferences("murmur", Context.MODE_PRIVATE)
                            .getString("device_name", null)
                }

                // When initialized but engine not yet available, poll the binder until
                // the service sets its engine (restartEngine re-calls onStartCommand but
                // does NOT re-trigger onServiceConnected, so we poll instead).
                LaunchedEffect(initialized) {
                    if (initialized) {
                        while (serviceEngine == null) {
                            delay(300)
                            serviceEngine = murmurService?.getEngine()
                        }
                    }
                }

                if (showDisconnectDialog) {
                    AlertDialog(
                        onDismissRequest = { showDisconnectDialog = false },
                        title = { Text("Disconnect") },
                        text = { Text("Disconnect ${if (deviceName != null) "\"$deviceName\"" else "this device"} from the network? You'll need the mnemonic to rejoin.") },
                        confirmButton = {
                            TextButton(onClick = {
                                showDisconnectDialog = false
                                murmurService?.disconnect()
                                serviceEngine = null
                                initialized = false
                            }) { Text("Disconnect") }
                        },
                        dismissButton = {
                            TextButton(onClick = { showDisconnectDialog = false }) { Text("Cancel") }
                        }
                    )
                }

                Scaffold(
                    topBar = {
                        if (engine != null) {
                            TopAppBar(
                                title = {
                                    Row(
                                        verticalAlignment = Alignment.CenterVertically,
                                        horizontalArrangement = Arrangement.spacedBy(8.dp)
                                    ) {
                                        Text(deviceName ?: "This device")
                                        Text(
                                            "Connected",
                                            style = MaterialTheme.typography.labelSmall,
                                            color = MaterialTheme.colorScheme.primary
                                        )
                                    }
                                },
                                actions = {
                                    IconButton(onClick = { showDisconnectDialog = true }) {
                                        Icon(Icons.Default.LinkOff, contentDescription = "Disconnect")
                                    }
                                }
                            )
                        }
                    },
                    bottomBar = {
                        if (engine != null) {
                            NavigationBar {
                                NavigationBarItem(
                                    selected = selectedTab == 0,
                                    onClick = { selectedTab = 0 },
                                    icon = { Icon(Icons.Default.Devices, "Devices") },
                                    label = { Text("Devices") }
                                )
                                NavigationBarItem(
                                    selected = selectedTab == 1,
                                    onClick = { selectedTab = 1 },
                                    icon = { Icon(Icons.Default.Folder, "Files") },
                                    label = { Text("Files") }
                                )
                                NavigationBarItem(
                                    selected = selectedTab == 2,
                                    onClick = { selectedTab = 2 },
                                    icon = { Icon(Icons.Default.Info, "Status") },
                                    label = { Text("Status") }
                                )
                            }
                        }
                    }
                ) { innerPadding ->
                    Box(
                        modifier = Modifier
                            .fillMaxSize()
                            .padding(innerPadding)
                    ) {
                        when {
                            !initialized -> SetupScreen(
                                onCreateNetwork = { devName, mnemonic ->
                                    murmurService?.initializeNetwork(devName, mnemonic)
                                    initialized = true
                                },
                                onJoinNetwork = { devName, mnemonic ->
                                    murmurService?.joinExistingNetwork(devName, mnemonic)
                                    initialized = true
                                }
                            )

                            // Initialized but engine not yet ready — service is starting.
                            engine == null -> Box(
                                modifier = Modifier.fillMaxSize(),
                                contentAlignment = Alignment.Center
                            ) { CircularProgressIndicator() }

                            selectedTab == 0 -> DeviceScreen(
                                viewModel = remember(engine) { DeviceViewModel(engine) },
                                myDeviceIdHex = engine.deviceIdHex()
                            )

                            selectedTab == 1 -> FileScreen(
                                viewModel = remember(engine) { FileViewModel(engine) }
                            )

                            else -> StatusScreen(
                                deviceIdHex = engine.deviceIdHex(),
                                events = engine.events
                            )
                        }
                    }
                }
            }
        }
    }

    override fun onDestroy() {
        unbindService(serviceConnection)
        super.onDestroy()
    }
}
