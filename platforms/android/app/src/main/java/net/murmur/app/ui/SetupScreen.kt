package net.murmur.app.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.unit.dp
import uniffi.murmur.newMnemonic

/**
 * First-run screen with a two-step wizard.
 *
 * Step 1: Choose between "Create Network" and "Join Network".
 * Step 2: Fill in the form (device name + mnemonic generation/input).
 */
@Composable
fun SetupScreen(
    onCreateNetwork: (deviceName: String, mnemonic: String) -> Unit,
    onJoinNetwork: (deviceName: String, mnemonic: String) -> Unit
) {
    // null = step 1 (choose mode), false = create form, true = join form
    var isJoining by remember { mutableStateOf<Boolean?>(null) }

    if (isJoining == null) {
        SetupChooseStep(
            onCreateChosen = { isJoining = false },
            onJoinChosen = { isJoining = true }
        )
    } else {
        SetupFormStep(
            isJoining = isJoining!!,
            onBack = { isJoining = null },
            onCreateNetwork = onCreateNetwork,
            onJoinNetwork = onJoinNetwork
        )
    }
}

/** Step 1: Choose between Create and Join. */
@Composable
private fun SetupChooseStep(
    onCreateChosen: () -> Unit,
    onJoinChosen: () -> Unit
) {
    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
        Text("Murmur", style = MaterialTheme.typography.headlineLarge)
        Text(
            "Private device sync",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )

        Spacer(Modifier.height(48.dp))

        Button(
            onClick = onCreateChosen,
            modifier = Modifier.fillMaxWidth()
        ) { Text("Create Network") }

        Spacer(Modifier.height(12.dp))

        OutlinedButton(
            onClick = onJoinChosen,
            modifier = Modifier.fillMaxWidth()
        ) { Text("Join Network") }
    }
}

/** Step 2: Device name + mnemonic form. */
@Composable
private fun SetupFormStep(
    isJoining: Boolean,
    onBack: () -> Unit,
    onCreateNetwork: (deviceName: String, mnemonic: String) -> Unit,
    onJoinNetwork: (deviceName: String, mnemonic: String) -> Unit
) {
    var deviceName by remember { mutableStateOf("") }
    var mnemonic by remember { mutableStateOf("") }
    var generatedMnemonic by remember { mutableStateOf<String?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    val context = LocalContext.current

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
        TextButton(
            onClick = onBack,
            modifier = Modifier.align(Alignment.Start)
        ) { Text("Back") }

        Text(
            if (isJoining) "Join Network" else "Create Network",
            style = MaterialTheme.typography.headlineMedium
        )

        Spacer(Modifier.height(24.dp))

        OutlinedTextField(
            value = deviceName,
            onValueChange = { deviceName = it },
            label = { Text("Device name") },
            placeholder = { Text("e.g. Max's Phone") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
            keyboardOptions = KeyboardOptions(
                capitalization = KeyboardCapitalization.Words,
                imeAction = ImeAction.Next
            )
        )

        Spacer(Modifier.height(16.dp))

        if (isJoining) {
            OutlinedTextField(
                value = mnemonic,
                onValueChange = { mnemonic = it },
                label = { Text("Mnemonic (12 or 24 words)") },
                placeholder = { Text("word1 word2 word3 …") },
                modifier = Modifier.fillMaxWidth(),
                minLines = 3,
                maxLines = 5
            )
        } else {
            Button(
                onClick = { generatedMnemonic = newMnemonic() },
                modifier = Modifier.fillMaxWidth()
            ) { Text("Generate Mnemonic") }

            generatedMnemonic?.let { m ->
                Spacer(Modifier.height(8.dp))
                Text(
                    "Write down your recovery phrase:",
                    style = MaterialTheme.typography.labelMedium
                )
                Spacer(Modifier.height(4.dp))
                Text(
                    m,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.primary,
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(vertical = 8.dp)
                )
                OutlinedButton(
                    onClick = {
                        val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                        clipboard.setPrimaryClip(ClipData.newPlainText("Murmur mnemonic", m))
                    },
                    modifier = Modifier.fillMaxWidth()
                ) { Text("Copy to clipboard") }
            }
        }

        error?.let {
            Spacer(Modifier.height(8.dp))
            Text(it, color = MaterialTheme.colorScheme.error)
        }

        Spacer(Modifier.height(24.dp))

        Button(
            onClick = {
                error = null
                if (deviceName.isBlank()) {
                    error = "Device name is required"
                    return@Button
                }
                try {
                    if (isJoining) {
                        if (mnemonic.isBlank()) {
                            error = "Mnemonic is required"
                            return@Button
                        }
                        onJoinNetwork(deviceName.trim(), mnemonic.trim())
                    } else {
                        val mn = generatedMnemonic ?: newMnemonic().also { generatedMnemonic = it }
                        onCreateNetwork(deviceName.trim(), mn)
                    }
                } catch (e: Exception) {
                    error = e.message
                }
            },
            modifier = Modifier.fillMaxWidth()
        ) {
            Text(
                if (isJoining) "Join"
                else if (generatedMnemonic == null) "Generate & Create"
                else "Create"
            )
        }
    }
}
