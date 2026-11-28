import 'dart:async';

import 'package:erika_flutter/erika_flutter.dart';
import 'package:material_ui/material_ui.dart';

void main() => runApp(const ErikaExampleApp());

class ErikaExampleApp extends StatelessWidget {
  const ErikaExampleApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Erika player',
      darkTheme: ThemeData.dark(),
      themeMode: ThemeMode.dark,
      home: const ErikaPlayerPage(),
    );
  }
}

class ErikaPlayerPage extends StatefulWidget {
  const ErikaPlayerPage({super.key});

  @override
  State<ErikaPlayerPage> createState() => _ErikaPlayerPageState();
}

class _ErikaPlayerPageState extends State<ErikaPlayerPage> {
  static const String _sampleUrl =
      'https://storage.googleapis.com/gtv-videos-bucket/sample/'
      'BigBuckBunny.mp4';

  final ErikaPlayer _player = ErikaPlayer();
  final TextEditingController _url = TextEditingController(text: _sampleUrl);
  String _status = 'Ready';

  Future<void> _run(String pending, Future<void> Function() action) async {
    setState(() => _status = pending);
    try {
      await action();
      if (mounted) {
        setState(() => _status = 'Playing');
      }
    } catch (error) {
      if (mounted) {
        setState(() => _status = 'Error: $error');
      }
    }
  }

  @override
  void dispose() {
    _url.dispose();
    unawaited(_player.dispose());
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Erika player')),
      body: SafeArea(
        child: Column(
          children: <Widget>[
            Expanded(
              child: ColoredBox(
                color: Colors.black,
                child: ErikaVideoView(player: _player),
              ),
            ),
            Padding(
              padding: const EdgeInsets.all(16),
              child: Column(
                children: <Widget>[
                  TextField(
                    controller: _url,
                    decoration: const InputDecoration(
                      border: OutlineInputBorder(),
                      labelText: 'Media URL',
                    ),
                  ),
                  const SizedBox(height: 12),
                  Row(
                    mainAxisAlignment: MainAxisAlignment.center,
                    children: <Widget>[
                      FilledButton(
                        onPressed: () => _run('Opening…', () async {
                          await _player.open(_url.text.trim());
                          await _player.play();
                        }),
                        child: const Text('Open'),
                      ),
                      const SizedBox(width: 8),
                      OutlinedButton(
                        onPressed: _player.play,
                        child: const Text('Play'),
                      ),
                      const SizedBox(width: 8),
                      OutlinedButton(
                        onPressed: _player.pause,
                        child: const Text('Pause'),
                      ),
                    ],
                  ),
                  const SizedBox(height: 8),
                  Text(_status),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}
